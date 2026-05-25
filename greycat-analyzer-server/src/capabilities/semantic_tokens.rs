//! Semantic tokens handler — per-identifier syntax-highlight classification
//! using resolver output. Walks the CST, classifies each token, then
//! delta-encodes the result per the LSP spec.
//!
//! Token `col` and `length` are emitted in the negotiated [`SourceEncoding`]'s
//! code-unit count, NOT raw bytes. Tree-sitter's `Point.column` /
//! `Node::byte_range().len()` are byte-based; under UTF-16 negotiation a line
//! containing non-ASCII characters would overflow its end (e.g. `///` doc
//! comment with `—` em-dash claims 81 units on a 79-unit line) and bleed
//! highlights onto the next line. The encoding-aware conversion lives in
//! the `push` closure so every classification arm (string / number / comment
//! / ident / keyword) benefits — no future caller can re-introduce the bug
//! by emitting raw bytes.

use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_analysis::stdlib::BUILTIN_RUNTIME_GLOBALS;
use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::walk_named;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens};

/// Token type table — must match `SEMANTIC_TOKEN_TYPES` registered with
/// the client.
pub const SEMANTIC_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::FUNCTION,
    SemanticTokenType::TYPE,
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::COMMENT,
    SemanticTokenType::KEYWORD,
];

const TOK_FN: u32 = 0;
const TOK_TYPE: u32 = 1;
const TOK_ENUM: u32 = 2;
const TOK_ENUM_MEMBER: u32 = 3;
const TOK_VAR: u32 = 4;
const TOK_PARAM: u32 = 5;
const TOK_STRING: u32 = 6;
const TOK_NUMBER: u32 = 7;
const TOK_COMMENT: u32 = 8;
const TOK_KEYWORD: u32 = 9;

pub fn semantic_tokens(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> SemanticTokens {
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);

    // Pre-split into line slices so per-token byte→units conversion is
    // O(line length) rather than O(text length). Includes a final empty
    // entry for the last `\n`-trailing slot so a token anchored at the
    // very end of the file still finds its line.
    let line_starts = compute_line_starts(text);

    let mut events: Vec<SemanticTokenEvent> = Vec::new();

    walk_named(root, |n| {
        let kind = n.kind();
        let push = |events: &mut Vec<SemanticTokenEvent>, ty: u32| {
            let p = n.start_position();
            let line_start = line_starts.get(p.row).copied().unwrap_or(text.len());
            let line_end = line_starts
                .get(p.row + 1)
                .map(|next| next.saturating_sub(1))
                .unwrap_or(text.len());
            let line_text = text.get(line_start..line_end).unwrap_or("");
            let start_byte_in_line = p.column;
            let node_byte_len = n.byte_range().len();
            let end_byte_in_line = start_byte_in_line + node_byte_len;
            let col = bytes_to_units(line_text, start_byte_in_line, encoding);
            let end = bytes_to_units(line_text, end_byte_in_line, encoding);
            events.push(SemanticTokenEvent {
                line: p.row as u32,
                col,
                length: end.saturating_sub(col),
                ty,
            });
        };
        match kind {
            // Don't paint the whole `string` node — its `string_substitution`
            // children hold real expressions (e.g. an `ident`) that need their
            // own token type. Overlapping ranges are forbidden by the LSP
            // semantic-tokens spec and break VSCode's rendering of the
            // interpolation. Tokenize only the literal string fragments;
            // the `"` quotes are anonymous and stay with textmate.
            "string_fragment" | "string_escape_sequence" => push(&mut events, TOK_STRING),
            // Same idea for numbers: the outer `number` (and inner
            // `number_suffixed`) nodes span both digit-runs AND the textual
            // type-suffix (`42_time`, `3day_2hour42s`, `3.14f`). Painting
            // the wrapper NUMBER hides the suffix as part of the literal.
            // Emit NUMBER only for the actual numeric segments, and a
            // distinct KEYWORD token for each `number_suffix` so themes
            // render the suffix differently from the digits.
            "number_int" | "number_decimal" | "number_scientific" => push(&mut events, TOK_NUMBER),
            "number_suffix" => push(&mut events, TOK_KEYWORD),
            "line_comment" | "doc_comment" => push(&mut events, TOK_COMMENT),
            // Language constants: paint as KEYWORD so themes render them
            // the same way they render `true` / `false` / language literals.
            // `this` rides along — without it the implicit-receiver inside
            // type methods reads as a plain ident (no LSP semantic token
            // fires, editors fall back to textmate which doesn't know it's
            // anything special).
            "null" | "true" | "false" | "this" => push(&mut events, TOK_KEYWORD),
            "ident" => {
                // Runtime value-position globals (`Infinity`, `NaN`) have no
                // `.gcl` decl — they resolve through `Definition::Project`,
                // which would otherwise paint them as TYPE. Override here
                // so they render as language constants.
                if text
                    .get(n.byte_range())
                    .is_some_and(|s| BUILTIN_RUNTIME_GLOBALS.iter().any(|(name, _)| *name == s))
                {
                    push(&mut events, TOK_KEYWORD);
                    return true;
                }
                // Enum-variant declaration sites: `enum E { A, B }` — the
                // `A` / `B` idents live as direct children of `enum_field`
                // and would otherwise resolve as `Decl::Enum` (their HIR
                // ident points back at the enclosing enum decl) and paint
                // TOK_ENUM. Emit ENUM_MEMBER instead so they match variant
                // *references* visually.
                if let Some(parent) = n.parent()
                    && parent.kind() == "enum_field"
                {
                    push(&mut events, TOK_ENUM_MEMBER);
                    return true;
                }
                // Enum-variant references (`MyEnum::Foo`) are CST-shaped as
                // `static_expr { receiver :: property }`. The property ident
                // doesn't bind to a resolver `Definition` (member access is
                // not a scope binding), so we'd otherwise emit nothing and
                // fall back to tree-sitter highlights. Detect the parent
                // shape, resolve the receiver chain's type, and emit
                // ENUM_MEMBER when it points at a `Decl::Enum`.
                if let Some(parent) = n.parent()
                    && parent.kind() == "static_expr"
                    && parent.child_by_field_name("property").map(|c| c.id()) == Some(n.id())
                {
                    let mut recv = parent.named_child(0);
                    while let Some(node) = recv
                        && node.kind() == "static_expr"
                    {
                        recv = node.named_child(0);
                    }
                    if let Some(type_ident) = recv
                        && type_ident.kind() == "type_ident"
                        && let Some(name_ident) = type_ident.child_by_field_name("name")
                        && let Some((recv_idx, _)) = hir
                            .idents
                            .iter()
                            .find(|(_, i)| i.byte_range == name_ident.byte_range())
                        && let Some(Definition::Decl(d)) = resolutions.lookup(recv_idx)
                        && matches!(&hir.decls[d], Decl::Enum(_))
                    {
                        push(&mut events, TOK_ENUM_MEMBER);
                        return true;
                    }
                }
                if let Some((idx, _)) = hir
                    .idents
                    .iter()
                    .find(|(_, i)| i.byte_range == n.byte_range())
                {
                    let ty = match resolutions.lookup(idx) {
                        Some(Definition::Decl(d)) => match &hir.decls[d] {
                            Decl::Fn(_) => TOK_FN,
                            Decl::Type(_) => TOK_TYPE,
                            Decl::Enum(_) => TOK_ENUM,
                            Decl::Var(_) => TOK_VAR,
                            Decl::Pragma(_) => TOK_KEYWORD,
                        },
                        Some(Definition::Local(_)) => TOK_VAR,
                        Some(Definition::Param(_)) => TOK_PARAM,
                        Some(Definition::Generic(_)) => TOK_TYPE,
                        Some(Definition::ProjectDecl { .. } | Definition::Project) => TOK_TYPE,
                        None => return true,
                    };
                    push(&mut events, ty);
                }
            }
            _ => {}
        }
        true
    });

    encode_semantic_tokens(events)
}

#[derive(Clone)]
struct SemanticTokenEvent {
    line: u32,
    col: u32,
    length: u32,
    ty: u32,
}

fn encode_semantic_tokens(mut events: Vec<SemanticTokenEvent>) -> SemanticTokens {
    events.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));
    let mut data = Vec::with_capacity(events.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for e in events {
        let delta_line = e.line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            e.col.saturating_sub(prev_col)
        } else {
            e.col
        };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: e.length,
            token_type: e.ty,
            token_modifiers_bitset: 0,
        });
        prev_line = e.line;
        prev_col = e.col;
    }
    SemanticTokens {
        result_id: None,
        data,
    }
}

/// Build a per-line start-byte table, where `line_starts[i]` is the byte
/// offset of line `i`'s first character. The trailing entry is the
/// total length plus one slot so `line_starts[row + 1]` always exists
/// for non-final rows.
fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Convert a byte-offset within a line slice to LSP code-unit count
/// under the negotiated encoding. UTF-8 → byte count (identity).
/// UTF-16 → walk chars, summing each one's `len_utf16()`.
fn bytes_to_units(line_text: &str, byte_offset_in_line: usize, encoding: SourceEncoding) -> u32 {
    match encoding {
        SourceEncoding::UTF8 => byte_offset_in_line as u32,
        SourceEncoding::UTF16 => {
            let mut units = 0u32;
            let mut bytes = 0usize;
            for ch in line_text.chars() {
                if bytes >= byte_offset_in_line {
                    break;
                }
                bytes += ch.len_utf8();
                units += ch.len_utf16() as u32;
            }
            units
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_syntax::parse;

    fn decode(tokens: &SemanticTokens) -> Vec<(u32, u32, u32, u32)> {
        let mut out = Vec::new();
        let mut line = 0u32;
        let mut col = 0u32;
        for t in &tokens.data {
            line += t.delta_line;
            if t.delta_line != 0 {
                col = 0;
            }
            col += t.delta_start;
            out.push((line, col, t.length, t.token_type));
        }
        out
    }

    #[test]
    fn doc_comment_with_em_dash_under_utf16_uses_code_units_not_bytes() {
        // `/// hello — world` — em-dash is 3 bytes UTF-8 but 1 UTF-16
        // code unit. Under UTF-16 the comment token must report length
        // 17 (matching the line's 17 code units), not 19 bytes — a
        // 19-unit token on a 17-unit line bleeds onto line 2 and paints
        // the `fn` keyword as comment. Regression for the editor bug
        // that motivated the encoding-aware refactor.
        let src = "/// hello — world\nfn main() {}\n";
        let tree = parse(src);
        let tokens = semantic_tokens(src, "project", tree.root_node(), SourceEncoding::UTF16);
        let events = decode(&tokens);
        let comment = events
            .iter()
            .find(|(_, _, _, ty)| *ty == TOK_COMMENT)
            .expect("doc_comment emits a COMMENT token");
        assert_eq!(comment.0, 0, "comment is on line 0");
        assert_eq!(comment.1, 0, "comment starts at col 0");
        assert_eq!(comment.2, 17, "comment length is 17 UTF-16 units");
    }

    #[test]
    fn doc_comment_with_em_dash_under_utf8_uses_byte_count() {
        // Same source under UTF-8: length must be 19 bytes.
        let src = "/// hello — world\nfn main() {}\n";
        let tree = parse(src);
        let tokens = semantic_tokens(src, "project", tree.root_node(), SourceEncoding::UTF8);
        let events = decode(&tokens);
        let comment = events
            .iter()
            .find(|(_, _, _, ty)| *ty == TOK_COMMENT)
            .expect("doc_comment emits a COMMENT token");
        assert_eq!(comment.2, 19, "comment length is 19 UTF-8 bytes");
    }
}
