//! LSP capability handlers (P3.*).
//!
//! Each function here takes the raw doc text + parsed tree (and any
//! extra args) and produces an LSP response value. They're wired up
//! from `server::main_loop` on receipt of the matching request method.
//!
//! Position handling: LSP positions are 0-indexed `(line, character)`
//! and the rest of this codebase treats `character` as a byte column
//! (matching tree-sitter's `Point.column`). All conversions go through
//! [`position_to_byte`] / [`byte_to_position`] for consistency.

use std::ops::Range;

use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr};
use greycat_analyzer_syntax::cst::{ancestors, node_at_offset, walk_named};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::*;

use crate::backend::semantic_diagnostics_for_text;

// =============================================================================
// Position helpers
// =============================================================================

pub(crate) fn position_to_byte(text: &str, pos: Position) -> usize {
    let mut line = 0u32;
    let mut byte = 0usize;
    for c in text.chars() {
        if line == pos.line {
            break;
        }
        byte += c.len_utf8();
        if c == '\n' {
            line += 1;
        }
    }
    // advance `character` byte columns, capping at next newline / EOF.
    let mut col = 0u32;
    let bytes = text.as_bytes();
    while col < pos.character && byte < bytes.len() {
        if bytes[byte] == b'\n' {
            break;
        }
        let c = text[byte..].chars().next().unwrap();
        byte += c.len_utf8();
        col += c.len_utf8() as u32;
    }
    byte
}

pub(crate) fn byte_to_position(text: &str, byte: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let prefix = &text[..byte.min(text.len())];
    for c in prefix.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf8() as u32;
        }
    }
    Position {
        line,
        character: col,
    }
}

pub(crate) fn byte_range_to_lsp(text: &str, range: &Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: byte_to_position(text, range.start),
        end: byte_to_position(text, range.end),
    }
}

// =============================================================================
// P3.1 — hover
// =============================================================================

/// Hover info at `pos`. Surfaces the inferred type of the expression
/// (from the analyzer's per-expression type table) or, for declaration
/// names, a short kind-line ("type Foo", "fn greet", etc.).
pub fn hover(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Option<Hover> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if !node.is_named() {
        return None;
    }

    // Build HIR + resolutions + analysis once. Cheap relative to the
    // current per-request rebuild model — incremental caching arrives
    // alongside salsa (P5.5) when profiling demands it.
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let analysis = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);

    // Strategy: walk ancestors looking for an HIR-known shape that
    // covers `node`'s byte range.
    let target_range = node.byte_range();
    let mut best: Option<(String, Range<usize>)> = None;

    for ancestor in ancestors(node) {
        let r = ancestor.byte_range();
        if r.start > target_range.start || r.end < target_range.end {
            break;
        }
        // Find an HIR Expr whose range matches.
        if let Some((expr_id, expr)) = hir
            .exprs
            .iter()
            .find(|(_, e)| e.byte_range() == r)
            && let Some(ty) = analysis.expr_types.get(&expr_id)
        {
            let label = format!(
                "{}: {}",
                short_expr_label(&hir, expr),
                greycat_analyzer_types::display(&analysis.types, *ty),
            );
            best = Some((label, r));
            break;
        }
    }

    if best.is_none() {
        // Fall back to a decl-name hover: walk module decls and find one
        // whose name range matches.
        if let Some(module) = hir.module.as_ref() {
            for decl_id in &module.decls {
                let decl = &hir.decls[*decl_id];
                if let Some(name_id) = decl.name() {
                    let name_range = hir.idents[name_id].byte_range.clone();
                    if name_range == node.byte_range() {
                        let kind_word = match decl {
                            Decl::Fn(_) => "fn",
                            Decl::Type(_) => "type",
                            Decl::Enum(_) => "enum",
                            Decl::Var(_) => "var",
                            Decl::Pragma(_) => "@",
                        };
                        let label =
                            format!("{kind_word} {}", hir.idents[name_id].text);
                        best = Some((label, name_range));
                        break;
                    }
                }
            }
        }
    }

    let (label, range) = best?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```greycat\n{label}\n```"),
        }),
        range: Some(byte_range_to_lsp(text, &range)),
    })
}

fn short_expr_label(hir: &Hir, expr: &Expr) -> String {
    match expr {
        Expr::Ident(idx) => hir.idents[*idx].text.clone(),
        Expr::Literal(_) => "literal".into(),
        Expr::String(_) => "string".into(),
        Expr::Call(_) => "call".into(),
        Expr::Binary(_) => "expression".into(),
        Expr::Unary(_) => "expression".into(),
        Expr::Member(m) | Expr::Arrow(m) => hir.idents[m.property].text.clone(),
        Expr::Static(s) => hir.idents[s.property].text.clone(),
        _ => "expression".into(),
    }
}

// =============================================================================
// P3.1 — signature help
// =============================================================================

pub fn signature_help(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Option<SignatureHelp> {
    let byte = position_to_byte(text, pos);
    let mut node = node_at_offset(root, byte)?;
    // Walk up looking for a `call_expr`.
    while node.kind() != "call_expr" {
        node = node.parent()?;
    }
    let callee = node.child_by_field_name("fn")?;
    let callee_text = text.get(callee.byte_range())?.to_string();

    let hir = lower_module(text, "module", lib, root);
    // Find a fn_decl with matching name.
    let module = hir.module.as_ref()?;
    let fnd = module.decls.iter().find_map(|d| match &hir.decls[*d] {
        Decl::Fn(f) if hir.idents[f.name].text == callee_text => Some(f.clone()),
        _ => None,
    })?;

    let mut params = Vec::new();
    let mut label = format!("fn {}(", hir.idents[fnd.name].text);
    for (i, p_id) in fnd.params.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        let p = &hir.fn_params[*p_id];
        let pname = hir.idents[p.name].text.clone();
        let label_start = label.len();
        let mut piece = pname.clone();
        if let Some(ty_id) = p.ty {
            let ty = &hir.type_refs[ty_id];
            piece.push_str(": ");
            piece.push_str(&hir.idents[ty.name].text);
            if ty.optional {
                piece.push('?');
            }
        }
        label.push_str(&piece);
        params.push(ParameterInformation {
            label: ParameterLabel::LabelOffsets([
                label_start as u32,
                (label_start + piece.len()) as u32,
            ]),
            documentation: None,
        });
    }
    label.push(')');
    if let Some(rt) = fnd.return_type {
        let r = &hir.type_refs[rt];
        label.push_str(": ");
        label.push_str(&hir.idents[r.name].text);
        if r.optional {
            label.push('?');
        }
    }

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation: fnd.doc.map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d,
                })
            }),
            parameters: Some(params),
            active_parameter: Some(0),
        }],
        active_signature: Some(0),
        active_parameter: Some(0),
    })
}

// =============================================================================
// P3.2 — goto definition
// =============================================================================

pub fn goto_definition(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }

    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);

    // Find which Idx<Ident> this CST node corresponds to.
    let ident_text = text.get(node.byte_range())?.to_string();
    let target = hir
        .idents
        .iter()
        .find(|(_, i)| i.byte_range == node.byte_range() && i.text == ident_text)?
        .0;

    let def = resolutions.lookup(target)?;
    let target_range = match def {
        Definition::Decl(decl_id) => {
            let name = hir.decls[decl_id].name()?;
            hir.idents[name].byte_range.clone()
        }
        Definition::Local(name) | Definition::Param(name) => hir.idents[name].byte_range.clone(),
        Definition::Builtin(_) => return None,
    };

    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: byte_range_to_lsp(text, &target_range),
    }))
}

// =============================================================================
// P3.3 — document symbols
// =============================================================================

pub fn document_symbols(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
) -> Vec<DocumentSymbol> {
    let hir = lower_module(text, "module", lib, root);
    let module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    for decl_id in &module.decls {
        let decl = &hir.decls[*decl_id];
        let Some(name_id) = decl.name() else {
            continue;
        };
        let ident = &hir.idents[name_id];
        let kind = match decl {
            Decl::Fn(_) => SymbolKind::FUNCTION,
            Decl::Type(_) => SymbolKind::CLASS,
            Decl::Enum(_) => SymbolKind::ENUM,
            Decl::Var(_) => SymbolKind::VARIABLE,
            Decl::Pragma(_) => SymbolKind::KEY,
        };
        let full_range = byte_range_to_lsp(text, decl.byte_range());
        let selection_range = byte_range_to_lsp(text, &ident.byte_range);
        let mut children: Vec<DocumentSymbol> = Vec::new();
        if let Decl::Type(td) = decl {
            for attr_id in &td.attrs {
                let a = &hir.type_attrs[*attr_id];
                let aname = &hir.idents[a.name];
                children.push(DocumentSymbol {
                    name: aname.text.clone(),
                    detail: None,
                    kind: SymbolKind::FIELD,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    range: byte_range_to_lsp(text, &a.byte_range),
                    selection_range: byte_range_to_lsp(text, &aname.byte_range),
                    children: None,
                });
            }
            for method_id in &td.methods {
                if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                    let mname = &hir.idents[fnd.name];
                    #[allow(deprecated)]
                    children.push(DocumentSymbol {
                        name: mname.text.clone(),
                        detail: None,
                        kind: SymbolKind::METHOD,
                        tags: None,
                        deprecated: None,
                        range: byte_range_to_lsp(text, &fnd.byte_range),
                        selection_range: byte_range_to_lsp(text, &mname.byte_range),
                        children: None,
                    });
                }
            }
        }
        #[allow(deprecated)]
        out.push(DocumentSymbol {
            name: ident.text.clone(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: full_range,
            selection_range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        });
    }
    out
}

// =============================================================================
// P3.4 — find references + rename
// =============================================================================

pub fn references(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Vec<Location> {
    let byte = position_to_byte(text, pos);
    let Some(node) = node_at_offset(root, byte) else {
        return Vec::new();
    };
    if node.kind() != "ident" {
        return Vec::new();
    }
    let target_text = text.get(node.byte_range()).unwrap_or("").to_string();
    if target_text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident"
            && text.get(n.byte_range()).unwrap_or("") == target_text
        {
            out.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(text, &n.byte_range()),
            });
        }
        true
    });
    let _ = lib; // future: cross-module references using lib metadata
    out
}

pub fn prepare_rename(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Option<PrepareRenameResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    let placeholder = text.get(node.byte_range())?.to_string();
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: byte_range_to_lsp(text, &node.byte_range()),
        placeholder,
    })
}

pub fn rename(
    text: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    let target_text = text.get(node.byte_range())?.to_string();
    let mut edits = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident"
            && text.get(n.byte_range()).unwrap_or("") == target_text
        {
            edits.push(TextEdit {
                range: byte_range_to_lsp(text, &n.byte_range()),
                new_text: new_name.to_string(),
            });
        }
        true
    });
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice
    let mut changes = std::collections::HashMap::new();
    changes.insert(uri.clone(), edits);
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

// =============================================================================
// P3.5 — document highlight + selection ranges + folding ranges
// =============================================================================

pub fn document_highlights(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
) -> Vec<DocumentHighlight> {
    let byte = position_to_byte(text, pos);
    let Some(node) = node_at_offset(root, byte) else {
        return Vec::new();
    };
    if node.kind() != "ident" {
        return Vec::new();
    }
    let target_text = text.get(node.byte_range()).unwrap_or("").to_string();
    if target_text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident"
            && text.get(n.byte_range()).unwrap_or("") == target_text
        {
            out.push(DocumentHighlight {
                range: byte_range_to_lsp(text, &n.byte_range()),
                kind: Some(DocumentHighlightKind::TEXT),
            });
        }
        true
    });
    out
}

pub fn selection_ranges(
    text: &str,
    root: tree_sitter::Node<'_>,
    positions: &[Position],
) -> Vec<SelectionRange> {
    positions
        .iter()
        .filter_map(|pos| {
            let byte = position_to_byte(text, *pos);
            let leaf = node_at_offset(root, byte)?;
            let mut head: Option<SelectionRange> = None;
            let chain: Vec<lsp_types::Range> = ancestors(leaf)
                .map(|n| byte_range_to_lsp(text, &n.byte_range()))
                .collect();
            for r in chain.into_iter().rev() {
                head = Some(SelectionRange {
                    range: r,
                    parent: head.map(Box::new),
                });
            }
            head
        })
        .collect()
}

pub fn folding_ranges(text: &str, root: tree_sitter::Node<'_>) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    walk_named(root, |n| {
        if matches!(n.kind(), "block" | "type_body" | "enum_body" | "object_initializers") {
            let r = n.byte_range();
            let start = byte_to_position(text, r.start);
            let end = byte_to_position(text, r.end);
            if end.line > start.line {
                out.push(FoldingRange {
                    start_line: start.line,
                    start_character: None,
                    end_line: end.line,
                    end_character: None,
                    kind: Some(FoldingRangeKind::Region),
                    collapsed_text: None,
                });
            }
        }
        true
    });
    out
}

// =============================================================================
// P3.6 — code actions
// =============================================================================

pub fn code_actions(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    range: lsp_types::Range,
) -> Vec<CodeActionOrCommand> {
    // Surface a single quickfix per current diagnostic in the requested
    // range: an empty placeholder edit. The LSP spec lets clients still
    // render the action even without an edit; this is the foundation
    // P4.2 / P3.6 will fill in with concrete fixes.
    let semantic = semantic_diagnostics_for_text(text, lib, root);
    semantic
        .into_iter()
        .filter(|d| ranges_overlap(&d.range, &range))
        .map(|d| {
            let title = format!("Quickfix: {}", d.message);
            CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![d.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some({
                        #[allow(clippy::mutable_key_type)]
                        let mut m = std::collections::HashMap::new();
                        m.insert(uri.clone(), vec![]);
                        m
                    }),
                    document_changes: None,
                    change_annotations: None,
                }),
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            })
        })
        .collect()
}

fn ranges_overlap(a: &lsp_types::Range, b: &lsp_types::Range) -> bool {
    !(a.end.line < b.start.line
        || a.start.line > b.end.line
        || (a.end.line == b.start.line && a.end.character < b.start.character)
        || (a.start.line == b.end.line && a.start.character > b.end.character))
}

// =============================================================================
// P3.7 — inlay hints
// =============================================================================

pub fn inlay_hints(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    range: &lsp_types::Range,
) -> Vec<InlayHint> {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let analysis = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);

    let module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let want = (
        position_to_byte(text, range.start),
        position_to_byte(text, range.end),
    );

    let mut out = Vec::new();
    for decl_id in &module.decls {
        if let Decl::Fn(fnd) = &hir.decls[*decl_id] {
            // Walk the body for `var name = expr;` shapes (no declared type).
            if let Some(body) = fnd.body {
                emit_var_hints(&hir, &analysis, body, want, text, &mut out);
            }
        }
    }
    out
}

fn emit_var_hints(
    hir: &Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    use greycat_analyzer_hir::types::Stmt;
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(stmts) => {
            for s in stmts {
                emit_var_hints(hir, analysis, *s, want, text, out);
            }
        }
        Stmt::Var(v) if v.ty.is_none() && v.init.is_some() => {
            let r = &v.byte_range;
            if r.end < want.0 || r.start > want.1 {
                return;
            }
            let init_id = v.init.unwrap();
            let Some(ty) = analysis.expr_types.get(&init_id).copied() else {
                return;
            };
            let label = format!(": {}", greycat_analyzer_types::display(&analysis.types, ty));
            // Anchor right after the variable name.
            let name_range = &hir.idents[v.name].byte_range;
            out.push(InlayHint {
                position: byte_to_position(text, name_range.end),
                label: InlayHintLabel::String(label),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            });
        }
        Stmt::If(i) => {
            emit_var_hints(hir, analysis, i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_var_hints(hir, analysis, eb, want, text, out);
            }
        }
        Stmt::While(w) => emit_var_hints(hir, analysis, w.body, want, text, out),
        Stmt::DoWhile(w) => emit_var_hints(hir, analysis, w.body, want, text, out),
        Stmt::For(f) => emit_var_hints(hir, analysis, f.body, want, text, out),
        Stmt::ForIn(f) => emit_var_hints(hir, analysis, f.body, want, text, out),
        Stmt::Try(t) => {
            emit_var_hints(hir, analysis, t.try_block, want, text, out);
            emit_var_hints(hir, analysis, t.catch_block, want, text, out);
        }
        Stmt::At(a) => emit_var_hints(hir, analysis, a.block, want, text, out),
        _ => {}
    }
}

// =============================================================================
// P3.8 — semantic tokens
// =============================================================================

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
) -> SemanticTokens {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);

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
            "string" => push(&mut events, TOK_STRING),
            "number" => push(&mut events, TOK_NUMBER),
            "line_comment" | "doc_comment" => push(&mut events, TOK_COMMENT),
            "ident" => {
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
                        Some(Definition::Builtin(_)) => TOK_TYPE,
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
