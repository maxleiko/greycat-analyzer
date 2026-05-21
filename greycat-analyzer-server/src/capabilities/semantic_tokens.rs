//! Semantic tokens handler — per-identifier syntax-highlight classification
//! using resolver output. Walks the CST, classifies each token, then
//! delta-encodes the result per the LSP spec.

use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_analysis::stdlib::BUILTIN_RUNTIME_GLOBALS;
use greycat_analyzer_core::SymbolTable;
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

pub fn semantic_tokens(text: &str, lib: &str, root: tree_sitter::Node<'_>) -> SemanticTokens {
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);

    let mut events: Vec<SemanticTokenEvent> = Vec::new();

    walk_named(root, |n| {
        let kind = n.kind();
        let push = |events: &mut Vec<SemanticTokenEvent>, ty: u32| {
            let p = n.start_position();
            let len = n.byte_range().len() as u32;
            events.push(SemanticTokenEvent {
                line: p.row as u32,
                col: p.column as u32,
                length: len,
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
