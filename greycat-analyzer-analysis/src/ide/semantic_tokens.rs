//! Semantic tokens — per-identifier syntax-highlight classification
//! using resolver output. Walks the CST, classifies each token, then
//! delta-encodes the result per the LSP spec.
//!
//! Token `col` and `length` are emitted in the negotiated
//! [`SourceEncoding`]'s code-unit count, NOT raw bytes. Tree-sitter's
//! `Point.column` / `Node::byte_range().len()` are byte-based; under
//! UTF-16 negotiation a line containing non-ASCII characters would
//! overflow its end (e.g. a `///` doc-comment with `—` em-dash claims
//! 81 units on a 79-unit line) and bleed highlights onto the next line.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::cst::walk_named;
use greycat_analyzer_syntax::tree_sitter;

use crate::resolver::{Definition, resolve};
use crate::stdlib::BUILTIN_RUNTIME_GLOBALS;

/// Token-type table — the LSP server registers `SemanticTokenType`
/// equivalents at the wire boundary; the wasm bridge exposes these
/// strings directly to Monaco for `SemanticTokensLegend.tokenTypes`.
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "function",
    "type",
    "enum",
    "enumMember",
    "variable",
    "parameter",
    "string",
    "number",
    "comment",
    "keyword",
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

/// Delta-encoded semantic-token stream, matching the LSP spec's
/// `data: Vec<u32>` quintuples (delta_line, delta_start, length,
/// token_type, token_modifiers_bitset). Stored as a flat `Vec<u32>`
/// since both the LSP wire format and Monaco's
/// `SemanticTokensProvider.provideDocumentSemanticTokens` consume the
/// same shape.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Default)]
pub struct SemanticTokens {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub data: Vec<u32>,
}

pub fn semantic_tokens(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    encoding: SourceEncoding,
) -> SemanticTokens {
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);

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
            "string_fragment" | "string_escape_sequence" => push(&mut events, TOK_STRING),
            "number_int" | "number_decimal" | "number_scientific" => push(&mut events, TOK_NUMBER),
            "number_suffix" => push(&mut events, TOK_KEYWORD),
            "line_comment" | "doc_comment" => push(&mut events, TOK_COMMENT),
            "null" | "true" | "false" | "this" => push(&mut events, TOK_KEYWORD),
            "ident" => {
                // Deprecated for-in query clause keyword (`sampling` / `limit`
                // / `skip`): a plain `ident` inside the grammar's
                // `for_in_clause`, but it reads as a keyword. Scoped to the three
                // recognized names to mirror the tree-sitter `highlights.scm`.
                if let Some(parent) = n.parent()
                    && parent.kind() == "for_in_clause"
                    && parent.child_by_field_name("keyword").map(|c| c.id()) == Some(n.id())
                    && text
                        .get(n.byte_range())
                        .is_some_and(|s| matches!(s, "sampling" | "limit" | "skip"))
                {
                    push(&mut events, TOK_KEYWORD);
                    return true;
                }
                if text
                    .get(n.byte_range())
                    .is_some_and(|s| BUILTIN_RUNTIME_GLOBALS.iter().any(|(name, _)| *name == s))
                {
                    push(&mut events, TOK_KEYWORD);
                    return true;
                }
                if let Some(parent) = n.parent()
                    && parent.kind() == "enum_field"
                {
                    push(&mut events, TOK_ENUM_MEMBER);
                    return true;
                }
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
    let mut data = Vec::with_capacity(events.len() * 5);
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;
    for e in events {
        let delta_line = e.line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            e.col.saturating_sub(prev_col)
        } else {
            e.col
        };
        data.push(delta_line);
        data.push(delta_start);
        data.push(e.length);
        data.push(e.ty);
        data.push(0); // token_modifiers_bitset (unused for now)
        prev_line = e.line;
        prev_col = e.col;
    }
    SemanticTokens { data }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

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
        // 5 u32s per token.
        for chunk in tokens.data.chunks(5) {
            let [delta_line, delta_start, length, token_type, _modifiers] = *chunk else {
                continue;
            };
            line += delta_line;
            if delta_line != 0 {
                col = 0;
            }
            col += delta_start;
            out.push((line, col, length, token_type));
        }
        out
    }

    #[test]
    fn doc_comment_with_em_dash_under_utf16_uses_code_units_not_bytes() {
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

    #[test]
    fn for_in_clause_keywords_classified_as_keyword_but_var_elsewhere() {
        // `skip` is a clause keyword on the for-in line, but an ordinary
        // variable on its own line — the semantic classification must follow
        // position, mirroring the contextual grammar.
        let src = "fn x(c: any) {\n    var skip = 1;\n    \
                   for (k, v in c skip 2 limit 5 sampling 3) {\n        \
                   println(skip);\n    }\n}\n";
        let tree = parse(src);
        let tokens = semantic_tokens(src, "project", tree.root_node(), SourceEncoding::UTF8);
        let events = decode(&tokens);
        // Line 2 is the for-in header: `skip` / `limit` / `sampling` are keywords.
        let kw_on_for_line = events
            .iter()
            .filter(|(l, _, _, ty)| *l == 2 && *ty == TOK_KEYWORD)
            .count();
        assert_eq!(
            kw_on_for_line, 3,
            "3 clause keywords on the for-in line; got {events:?}"
        );
        // Line 1 is `var skip = 1;` — here `skip` is a variable, not a keyword.
        let kw_on_var_line = events
            .iter()
            .filter(|(l, _, _, ty)| *l == 1 && *ty == TOK_KEYWORD)
            .count();
        assert_eq!(
            kw_on_var_line, 0,
            "`var skip` is a variable, not a keyword; got {events:?}"
        );
    }
}
