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

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::lint::{LintSeverity, run_lints};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr};
use greycat_analyzer_syntax::cst::{ancestors, node_at_offset, walk_named};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::*;

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

/// Hover info at `pos`. Three layers of lookup:
/// 1. Cursor on an `ident` node — surface the resolver `Definition`'s
///    binding info (param/local type, decl kind, builtin name).
/// 2. Cursor inside a non-ident HIR `Expr` — surface
///    `<short-label>: <inferred-type>`.
/// 3. No HIR shape covers the cursor — return `None`.
pub fn hover(text: &str, lib: &str, root: tree_sitter::Node<'_>, pos: Position) -> Option<Hover> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if !node.is_named() {
        return None;
    }

    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let analysis = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);

    // --- Layer 1: ident-based hover (params / locals / decls / builtins).
    if node.kind() == "ident"
        && let Some((ident_idx, ident)) = hir
            .idents
            .iter()
            .find(|(_, i)| i.byte_range == node.byte_range())
    {
        if let Some(label) = ident_hover_label(&hir, &resolutions, &analysis, ident_idx, ident) {
            return Some(hover_markup(label, ident.byte_range.clone(), text));
        }
        // Decl-defining ident (e.g. cursor on the `helper` in `fn helper()`).
        if let Some(module) = hir.module.as_ref() {
            for decl_id in &module.decls {
                let decl = &hir.decls[*decl_id];
                if let Some(name_id) = decl.name()
                    && hir.idents[name_id].byte_range == node.byte_range()
                {
                    let label = format!("{} {}", decl_kind_word(decl), ident.text);
                    return Some(hover_markup(
                        label,
                        hir.idents[name_id].byte_range.clone(),
                        text,
                    ));
                }
            }
        }
    }

    // --- Layer 2: non-ident expression hover.
    let target_range = node.byte_range();
    for ancestor in ancestors(node) {
        let r = ancestor.byte_range();
        if r.start > target_range.start || r.end < target_range.end {
            break;
        }
        if let Some((expr_id, expr)) = hir
            .exprs
            .iter()
            .filter(|(_, e)| !matches!(e, Expr::Ident(_)))
            .find(|(_, e)| {
                let er = e.byte_range();
                !er.is_empty() && er == r
            })
            && let Some(ty) = analysis.expr_types.get(&expr_id)
        {
            let label = format!(
                "{}: {}",
                short_expr_label(&hir, expr),
                greycat_analyzer_types::display(&analysis.types, *ty),
            );
            return Some(hover_markup(label, r, text));
        }
    }

    None
}

fn ident_hover_label(
    hir: &Hir,
    resolutions: &greycat_analyzer_analysis::resolver::Resolutions,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    ident_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
    ident: &greycat_analyzer_hir::types::Ident,
) -> Option<String> {
    // P6.3: property idents in `a.b` aren't in `Resolutions` — they
    // bind to a `TypeAttr` / method via the analyzer's member pass.
    // Check that first so member hovers render with the right shape.
    if let Some(member) = analysis.member_lookup(ident_idx) {
        return Some(member_hover_label(hir, analysis, member, ident));
    }
    match resolutions.lookup(ident_idx)? {
        Definition::Param(name) | Definition::Local(name) => {
            analysis.def_types.get(&name).map(|ty| {
                format!(
                    "{}: {}",
                    ident.text,
                    greycat_analyzer_types::display(&analysis.types, *ty),
                )
            })
        }
        Definition::Decl(decl_id) => Some(format!(
            "{} {}",
            decl_kind_word(&hir.decls[decl_id]),
            ident.text
        )),
        Definition::Generic(_) => Some(format!("(type parameter) {}", ident.text)),
        Definition::ProjectDecl { .. } | Definition::Project => {
            Some(format!("(project) {}", ident.text))
        }
    }
}

fn decl_kind_word(decl: &Decl) -> &'static str {
    match decl {
        Decl::Fn(_) => "fn",
        Decl::Type(_) => "type",
        Decl::Enum(_) => "enum",
        Decl::Var(_) => "var",
        Decl::Pragma(_) => "@",
    }
}

/// Hover label for a property ident bound by P6.3 member resolution
/// (`a.b` / `a->b`). Renders attribute / method shape with the
/// declared / inferred return type when available.
fn member_hover_label(
    hir: &Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    member: greycat_analyzer_analysis::analyzer::MemberDef,
    ident: &greycat_analyzer_hir::types::Ident,
) -> String {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    match member {
        MemberDef::Attr(attr_id) => {
            let attr = &hir.type_attrs[attr_id];
            if let Some(ty_ref) = attr.ty {
                let name = &hir.idents[hir.type_refs[ty_ref].name].text;
                format!("{}: {}", ident.text, name)
            } else {
                format!("attr {}", ident.text)
            }
        }
        MemberDef::Method(decl_id) => {
            // Capture the function-shape: name + return type if declared.
            let Decl::Fn(fnd) = &hir.decls[decl_id] else {
                return format!("fn {}", ident.text);
            };
            let ret = fnd
                .return_type
                .map(|r| hir.idents[hir.type_refs[r].name].text.clone());
            // Suppress unused-import-of-analysis warning by referencing it.
            let _ = analysis;
            match ret {
                Some(rt) => format!("fn {}(): {}", ident.text, rt),
                None => format!("fn {}()", ident.text),
            }
        }
    }
}

fn hover_markup(label: String, range: Range<usize>, text: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```greycat\n{label}\n```"),
        }),
        range: Some(byte_range_to_lsp(text, &range)),
    }
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

    if let Some(def) = resolutions.lookup(target) {
        let target_range = match def {
            Definition::Decl(decl_id) => {
                let name = hir.decls[decl_id].name()?;
                hir.idents[name].byte_range.clone()
            }
            Definition::Local(name) | Definition::Param(name) | Definition::Generic(name) => {
                hir.idents[name].byte_range.clone()
            }
            // P11.2 records the cross-module decl pointer here, but
            // resolving it to a `Location` requires reading the foreign
            // module's text — that's P11.3. For now fall through so
            // the member-access lookup below still runs.
            Definition::ProjectDecl { .. } | Definition::Project => return None,
        };
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: byte_range_to_lsp(text, &target_range),
        }));
    }

    // P6.3: the property side of `a.b` / `a->b` isn't in `Resolutions`
    // — bindings live in `AnalysisResult::member_uses`. Run the
    // analyzer to consult it before giving up.
    let analysis = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);
    let member = analysis.member_lookup(target)?;
    let target_range = match member {
        greycat_analyzer_analysis::analyzer::MemberDef::Attr(attr_id) => {
            let name = hir.type_attrs[attr_id].name;
            hir.idents[name].byte_range.clone()
        }
        greycat_analyzer_analysis::analyzer::MemberDef::Method(decl_id) => {
            let name = hir.decls[decl_id].name()?;
            hir.idents[name].byte_range.clone()
        }
    };
    Some(GotoDefinitionResponse::Scalar(Location {
        uri: uri.clone(),
        range: byte_range_to_lsp(text, &target_range),
    }))
}

/// P11.3 — turn a `Definition::ProjectDecl { uri, decl }` into the
/// concrete `Location` of the foreign module's decl-name range. Pure
/// helper: caller fetches the foreign HIR + text from the project-
/// analysis cache + source manager and passes them in.
pub fn cross_module_decl_location(
    foreign_uri: &Uri,
    foreign_text: &str,
    foreign_hir: &Hir,
    decl_id: greycat_analyzer_hir::arena::Idx<Decl>,
) -> Option<Location> {
    let name_id = foreign_hir.decls[decl_id].name()?;
    let range = byte_range_to_lsp(foreign_text, &foreign_hir.idents[name_id].byte_range);
    Some(Location {
        uri: foreign_uri.clone(),
        range,
    })
}

/// P11.5 — turn a `ForeignMember` (cross-module attr / method
/// binding) into a `Location` pointing at the foreign attr / method's
/// name range. Mirrors [`cross_module_decl_location`] but indexes
/// `type_attrs` for `MemberDef::Attr` and `decls` for `Method`.
pub fn cross_module_member_location(
    foreign_uri: &Uri,
    foreign_text: &str,
    foreign_hir: &Hir,
    member: &greycat_analyzer_analysis::analyzer::MemberDef,
) -> Option<Location> {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    let range = match *member {
        MemberDef::Attr(attr_id) => {
            let name_id = foreign_hir.type_attrs[attr_id].name;
            foreign_hir.idents[name_id].byte_range.clone()
        }
        MemberDef::Method(decl_id) => {
            let name_id = foreign_hir.decls[decl_id].name()?;
            foreign_hir.idents[name_id].byte_range.clone()
        }
    };
    Some(Location {
        uri: foreign_uri.clone(),
        range: byte_range_to_lsp(foreign_text, &range),
    })
}

/// P11.3 helper — map a cursor position in `text` to its `Idx<Ident>`
/// against the cached `hir`'s `idents` arena, by byte-range match.
/// Returns `None` if the cursor isn't over an ident or no matching
/// idx was allocated (e.g. lowering skipped this shape).
pub fn cursor_ident_idx(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    hir: &Hir,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    idx_for_node(hir, node)
}

/// P8.6 — `textDocument/implementation`. For a method-name ident,
/// returns every concrete (non-`abstract`, non-`native`) method with
/// that name across all type decls in the module. For other idents,
/// falls through to [`goto_definition`] so the editor still produces
/// a useful jump.
pub fn goto_implementation(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return goto_definition(text, lib, root, uri, pos);
    }
    let cursor_text = text.get(node.byte_range())?.to_string();

    let hir = lower_module(text, "module", lib, root);
    let mut locations = Vec::new();
    let Some(module) = hir.module.as_ref() else {
        return goto_definition(text, lib, root, uri, pos);
    };
    for decl_id in &module.decls {
        if let Decl::Type(td) = &hir.decls[*decl_id] {
            for method_id in &td.methods {
                if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                    if fnd.modifiers.abstract_ || fnd.modifiers.native {
                        continue;
                    }
                    if hir.idents[fnd.name].text == cursor_text {
                        locations.push(Location {
                            uri: uri.clone(),
                            range: byte_range_to_lsp(text, &hir.idents[fnd.name].byte_range),
                        });
                    }
                }
            }
        }
    }
    if locations.is_empty() {
        return goto_definition(text, lib, root, uri, pos);
    }
    Some(GotoDefinitionResponse::Array(locations))
}

/// P11.6 — project-wide `textDocument/implementation`. Walks every
/// cached module's `TypeDecl::methods` for concrete (non-`abstract`,
/// non-`native`) methods whose name matches the cursor's ident text.
/// Falls through to in-module [`goto_implementation`] (which itself
/// falls through to [`goto_definition`]) for non-method idents and
/// when no project-wide method match is found.
pub fn goto_implementation_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<GotoDefinitionResponse> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let byte = position_to_byte(&doc.text, cursor_pos);
    let node = node_at_offset(doc.root_node(), byte)?;
    if node.kind() != "ident" {
        return goto_implementation(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos);
    }
    let cursor_text = doc.text.get(node.byte_range())?.to_string();
    drop(doc);

    let mut locations = Vec::new();
    for (uri, module) in project.iter() {
        let Some(module_root) = module.hir.module.as_ref() else {
            continue;
        };
        let Some(other_cell) = manager.get(uri) else {
            continue;
        };
        let other_doc = other_cell.borrow();
        for decl_id in &module_root.decls {
            let Decl::Type(td) = &module.hir.decls[*decl_id] else {
                continue;
            };
            for method_id in &td.methods {
                let Decl::Fn(fnd) = &module.hir.decls[*method_id] else {
                    continue;
                };
                if fnd.modifiers.abstract_ || fnd.modifiers.native {
                    continue;
                }
                if module.hir.idents[fnd.name].text == cursor_text {
                    locations.push(Location {
                        uri: uri.clone(),
                        range: byte_range_to_lsp(
                            &other_doc.text,
                            &module.hir.idents[fnd.name].byte_range,
                        ),
                    });
                }
            }
        }
    }
    if locations.is_empty() {
        let cell = manager.get(cursor_uri)?;
        let doc = cell.borrow();
        return goto_implementation(&doc.text, &doc.lib, doc.root_node(), cursor_uri, cursor_pos);
    }
    Some(GotoDefinitionResponse::Array(locations))
}

// =============================================================================
// P3.3 — document symbols
// =============================================================================

pub fn document_symbols(text: &str, lib: &str, root: tree_sitter::Node<'_>) -> Vec<DocumentSymbol> {
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

    // P8.1 scope-aware filter: resolve the cursor ident's binding via
    // `Resolutions`, then collect every use whose `Definition` points
    // back at the same binding. Falls back to text equality for
    // `Definition::Project` (cross-module — P8.2 lifts it through the
    // project pipeline).
    let hir = lower_module(text, "module", lib, root);
    let res = resolve(&hir);
    let Some(cursor_idx) = idx_for_node(&hir, node) else {
        return Vec::new();
    };
    let Some(target) = target_binding(&hir, &res, cursor_idx) else {
        // Cross-module / unresolved: fall back to text equality so the
        // capability doesn't go silent on stdlib symbols.
        return references_by_text(text, root, node, uri);
    };

    let mut out = Vec::new();
    // Include the binding site itself.
    out.push(Location {
        uri: uri.clone(),
        range: byte_range_to_lsp(text, &hir.idents[target].byte_range),
    });
    for (use_idx, def) in &res.uses {
        let resolves_to = match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
            Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
            // Cross-module `ProjectDecl` use sites point at a foreign
            // HIR — they don't share the local `target` ident. P11.4
            // walks every doc's `Resolutions` to filter these by URI.
            Definition::ProjectDecl { .. } | Definition::Project => None,
        };
        if resolves_to == Some(target) {
            out.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(text, &hir.idents[*use_idx].byte_range),
            });
        }
    }
    out
}

/// Map a tree-sitter ident node back to its `Idx<Ident>` in the HIR
/// arena by byte-range match. Returns `None` if no matching ident was
/// allocated (e.g., the lowering skipped this shape).
fn idx_for_node(
    hir: &Hir,
    node: tree_sitter::Node<'_>,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    hir.idents
        .iter()
        .find(|(_, i)| i.byte_range == node.byte_range())
        .map(|(idx, _)| idx)
}

/// Resolve `cursor_idx` to the *binding* `Idx<Ident>` (the def site
/// the resolver would point at). Returns `None` for `Project` /
/// unresolved idents — caller decides the fallback.
fn target_binding(
    hir: &Hir,
    res: &greycat_analyzer_analysis::resolver::Resolutions,
    cursor_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>> {
    if let Some(def) = res.uses.get(&cursor_idx) {
        return match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
            Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
            Definition::ProjectDecl { .. } | Definition::Project => None,
        };
    }
    // Not a use site — cursor is on a binding. Treat the cursor as the
    // binding itself.
    Some(cursor_idx)
}

/// Pre-P8.1 text-equality fallback. Used when the cursor doesn't
/// resolve through `Resolutions` (e.g., cross-module names) so the
/// capability still returns useful results.
fn references_by_text(
    text: &str,
    root: tree_sitter::Node<'_>,
    cursor_node: tree_sitter::Node<'_>,
    uri: &Uri,
) -> Vec<Location> {
    let target_text = text.get(cursor_node.byte_range()).unwrap_or("").to_string();
    if target_text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk_named(root, |n| {
        if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
            out.push(Location {
                uri: uri.clone(),
                range: byte_range_to_lsp(text, &n.byte_range()),
            });
        }
        true
    });
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

    // P8.1: same scope-aware filter as `references`. Falls back to
    // text equality when the cursor name doesn't resolve through
    // `Resolutions` (cross-module — P8.2 picks that up).
    let hir = lower_module(text, "module", "project", root);
    let res = resolve(&hir);
    let mut edits = Vec::new();
    if let Some(cursor_idx) = idx_for_node(&hir, node)
        && let Some(target) = target_binding(&hir, &res, cursor_idx)
    {
        edits.push(TextEdit {
            range: byte_range_to_lsp(text, &hir.idents[target].byte_range),
            new_text: new_name.to_string(),
        });
        for (use_idx, def) in &res.uses {
            let resolves_to = match def {
                Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => Some(*i),
                Definition::Decl(decl_id) => hir.decls[*decl_id].name(),
                Definition::ProjectDecl { .. } | Definition::Project => None,
            };
            if resolves_to == Some(target) {
                edits.push(TextEdit {
                    range: byte_range_to_lsp(text, &hir.idents[*use_idx].byte_range),
                    new_text: new_name.to_string(),
                });
            }
        }
    } else {
        // Fallback: text equality for unresolvable / cross-module names.
        let target_text = text.get(node.byte_range())?.to_string();
        walk_named(root, |n| {
            if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
                edits.push(TextEdit {
                    range: byte_range_to_lsp(text, &n.byte_range()),
                    new_text: new_name.to_string(),
                });
            }
            true
        });
    }
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
// P11.4 — project-wide references + rename
// =============================================================================

/// What the cursor is asking us to rename / find references for.
/// Returned by [`resolve_rename_target`] and consumed by
/// [`references_across_project`] / [`rename_across_project`].
#[derive(Debug, Clone)]
pub enum RenameTarget {
    /// Function parameter / local var / generic-param. Confined to its
    /// declaring module's scope — no cross-module fan-out.
    LocalIdent {
        uri: Uri,
        ident: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
    },
    /// Top-level decl. May be referenced from any module via
    /// [`Definition::Decl`] (in the home module) or
    /// [`Definition::ProjectDecl`] (importers).
    ProjectDecl {
        uri: Uri,
        decl: greycat_analyzer_hir::arena::Idx<Decl>,
    },
}

/// Inspect the cursor's binding through cached project analysis and
/// classify the rename / reference target. Returns `None` for cursors
/// not on an ident, runtime-only names ([`Definition::Project`] —
/// `Array`, `Map`, native fns, primitives), and unrecognized binding
/// shapes (e.g. method names — that's P11.5 / P11.6 territory).
pub fn resolve_rename_target(
    project: &ProjectAnalysis,
    cursor_uri: &Uri,
    cursor_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<RenameTarget> {
    let module = project.module(cursor_uri)?;
    if let Some(def) = module.resolutions.lookup(cursor_idx) {
        return match def {
            Definition::Param(i) | Definition::Local(i) | Definition::Generic(i) => {
                Some(RenameTarget::LocalIdent {
                    uri: cursor_uri.clone(),
                    ident: i,
                })
            }
            Definition::Decl(decl) => Some(RenameTarget::ProjectDecl {
                uri: cursor_uri.clone(),
                decl,
            }),
            Definition::ProjectDecl { uri, decl } => Some(RenameTarget::ProjectDecl { uri, decl }),
            // Runtime-only names (Array / Map / node tags / native fns
            // / primitives) have no declaration to rename.
            Definition::Project => None,
        };
    }
    // Cursor isn't a use site — it's on a binding. Top-level decl
    // names appear in `module.decls`; everything else (param names,
    // local var names, generic-param decls) treats as LocalIdent.
    let module_root = module.hir.module.as_ref()?;
    for &decl_id in &module_root.decls {
        if module.hir.decls[decl_id].name() == Some(cursor_idx) {
            return Some(RenameTarget::ProjectDecl {
                uri: cursor_uri.clone(),
                decl: decl_id,
            });
        }
    }
    Some(RenameTarget::LocalIdent {
        uri: cursor_uri.clone(),
        ident: cursor_idx,
    })
}

/// P11.4 — find every reference to the cursor's binding across the
/// whole project. Replaces the previous text-equality fallback.
pub fn references_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Vec<Location> {
    let Some(target) = cursor_target(project, manager, cursor_uri, cursor_pos) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    visit_target_sites(project, manager, &target, |uri, text, range| {
        out.push(Location {
            uri: uri.clone(),
            range: byte_range_to_lsp(text, &range),
        });
    });
    out
}

/// P11.4 — produce a `WorkspaceEdit` renaming every site the cursor's
/// binding is referenced from, across the whole project.
pub fn rename_across_project(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let target = cursor_target(project, manager, cursor_uri, cursor_pos)?;
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
    let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
        std::collections::HashMap::new();
    visit_target_sites(project, manager, &target, |uri, text, range| {
        changes.entry(uri.clone()).or_default().push(TextEdit {
            range: byte_range_to_lsp(text, &range),
            new_text: new_name.to_string(),
        });
    });
    if changes.is_empty() {
        return None;
    }
    Some(WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    })
}

fn cursor_target(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    cursor_uri: &Uri,
    cursor_pos: Position,
) -> Option<RenameTarget> {
    let cell = manager.get(cursor_uri)?;
    let doc = cell.borrow();
    let module = project.module(cursor_uri)?;
    let cursor_idx = cursor_ident_idx(&doc.text, doc.root_node(), cursor_pos, &module.hir)?;
    drop(doc);
    resolve_rename_target(project, cursor_uri, cursor_idx)
}

/// Walk every site the rename target is referenced from. Calls `emit`
/// with `(home_uri, home_text, byte_range)` for each hit — emit may
/// shape it into a `Location`, `TextEdit`, etc.
fn visit_target_sites(
    project: &ProjectAnalysis,
    manager: &SourceManager,
    target: &RenameTarget,
    mut emit: impl FnMut(&Uri, &str, Range<usize>),
) {
    match target {
        RenameTarget::LocalIdent { uri, ident } => {
            let Some(cell) = manager.get(uri) else {
                return;
            };
            let doc = cell.borrow();
            let Some(module) = project.module(uri) else {
                return;
            };
            // Binding site.
            emit(uri, &doc.text, module.hir.idents[*ident].byte_range.clone());
            for (use_idx, def) in &module.resolutions.uses {
                let hits = matches!(
                    def,
                    Definition::Param(i) | Definition::Local(i) | Definition::Generic(i)
                        if i == ident
                );
                if hits {
                    emit(
                        uri,
                        &doc.text,
                        module.hir.idents[*use_idx].byte_range.clone(),
                    );
                }
            }
        }
        RenameTarget::ProjectDecl {
            uri: target_uri,
            decl: target_decl,
        } => {
            // Home module: binding site + same-module Decl uses.
            if let Some(home_cell) = manager.get(target_uri)
                && let Some(home_module) = project.module(target_uri)
            {
                let home_doc = home_cell.borrow();
                if let Some(name_idx) = home_module.hir.decls[*target_decl].name() {
                    emit(
                        target_uri,
                        &home_doc.text,
                        home_module.hir.idents[name_idx].byte_range.clone(),
                    );
                }
                for (use_idx, def) in &home_module.resolutions.uses {
                    if matches!(def, Definition::Decl(d) if d == target_decl) {
                        emit(
                            target_uri,
                            &home_doc.text,
                            home_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
            // Importers: every other module's ProjectDecl uses with
            // matching (uri, decl).
            for (other_uri, other_module) in project.iter() {
                if other_uri == target_uri {
                    continue;
                }
                let Some(other_cell) = manager.get(other_uri) else {
                    continue;
                };
                let other_doc = other_cell.borrow();
                for (use_idx, def) in &other_module.resolutions.uses {
                    if let Definition::ProjectDecl { uri, decl } = def
                        && uri == target_uri
                        && decl == target_decl
                    {
                        emit(
                            other_uri,
                            &other_doc.text,
                            other_module.hir.idents[*use_idx].byte_range.clone(),
                        );
                    }
                }
            }
        }
    }
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
        if n.kind() == "ident" && text.get(n.byte_range()).unwrap_or("") == target_text {
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
        if matches!(
            n.kind(),
            "block" | "type_body" | "enum_body" | "object_initializers"
        ) {
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
    // P8.3: emit concrete `TextEdit`s for fixable diagnostics. The
    // synthesizer maps the diagnostic's `code` to a fix shape:
    //   - `missing-token` → insert the missing token at the gap
    //   - `unused-local` / `unused-decl` → "remove" by collapsing the
    //     declaring statement (best-effort — gives the user a single-
    //     click delete)
    //   - `unused-param` → prepend `_` to the parameter name
    let semantic = current_diagnostics(text, lib, root);
    semantic
        .into_iter()
        .filter(|d| ranges_overlap(&d.range, &range))
        .map(|d| {
            let edits = synthesize_fix(text, &d);
            let title = match edits.is_empty() {
                true => format!("Quickfix: {}", d.message),
                false => format!("Fix: {}", d.message),
            };
            CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![d.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some({
                        #[allow(clippy::mutable_key_type)]
                        let mut m = std::collections::HashMap::new();
                        m.insert(uri.clone(), edits);
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

/// Map a diagnostic to a concrete `Vec<TextEdit>` (P8.3). Returns an
/// empty vec when no automatic fix is known for this diagnostic shape.
fn synthesize_fix(text: &str, diag: &Diagnostic) -> Vec<TextEdit> {
    let code = match &diag.code {
        Some(NumberOrString::String(s)) => s.as_str(),
        _ => return Vec::new(),
    };
    match code {
        "missing-token" => {
            // The diagnostic message is "missing `<kind>`". Pluck the
            // token between backticks and insert it at the diagnostic's
            // start position (a zero-width range).
            let Some(token) = diag
                .message
                .split_once('`')
                .and_then(|(_, rest)| rest.split_once('`').map(|(t, _)| t))
            else {
                return Vec::new();
            };
            vec![TextEdit {
                range: lsp_types::Range {
                    start: diag.range.start,
                    end: diag.range.start,
                },
                new_text: token.to_string(),
            }]
        }
        "unused-local" | "unused-decl" => {
            // Best-effort delete: replace the diagnostic's range with
            // empty text. Caller's editor will collapse the resulting
            // empty line; full statement-level deletion lives in P8.4
            // (lint-fix driver) where we have HIR context.
            vec![TextEdit {
                range: diag.range,
                new_text: String::new(),
            }]
        }
        "unused-param" => {
            // Prepend `_` to opt out of the rule. Read the source
            // text at the diagnostic range to produce `_<name>`.
            let start = position_to_byte(text, diag.range.start);
            let end = position_to_byte(text, diag.range.end);
            if end <= start || end > text.len() {
                return Vec::new();
            }
            let name = &text[start..end];
            vec![TextEdit {
                range: diag.range,
                new_text: format!("_{name}"),
            }]
        }
        _ => Vec::new(),
    }
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
            // P13.7: return-type hint when the fn has no declared
            // return type but the analyzer inferred one from the body.
            if fnd.return_type.is_none()
                && let Some(body) = fnd.body
                && let Some(ty) = inferred_fn_return(&hir, &analysis, body)
            {
                let name_range = &hir.idents[fnd.name].byte_range;
                if name_range.start <= want.1 && name_range.end >= want.0 {
                    let label =
                        format!(": {}", greycat_analyzer_types::display(&analysis.types, ty));
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
            }
            // Walk the body for `var name = expr;` shapes (no declared type).
            if let Some(body) = fnd.body {
                emit_var_hints(&hir, &analysis, body, want, text, &mut out);
                // P13.7: argument-name hints inside the body.
                emit_call_arg_hints(&hir, &resolutions, body, want, text, &mut out);
            }
        }
    }
    out
}

/// P13.7 — peek at the last expression-shaped statement of a fn body
/// to infer its return type. Returns `None` for blocks that don't end
/// in a `Stmt::Return(...)` with an inferred-type expression.
fn inferred_fn_return(
    hir: &Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    body: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
) -> Option<greycat_analyzer_types::TypeId> {
    use greycat_analyzer_hir::types::Stmt;
    let stmts = match &hir.stmts[body] {
        Stmt::Block(s) => s,
        _ => return None,
    };
    for s in stmts.iter().rev() {
        if let Stmt::Return(Some(e)) = &hir.stmts[*s] {
            return analysis.expr_types.get(e).copied();
        }
    }
    None
}

/// P13.7 — walk the body for `Expr::Call` and emit one
/// `<param_name>:` hint anchored at the start of each positional arg.
fn emit_call_arg_hints(
    hir: &Hir,
    resolutions: &greycat_analyzer_analysis::resolver::Resolutions,
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
                emit_call_arg_hints(hir, resolutions, *s, want, text, out);
            }
        }
        Stmt::Expr(e)
        | Stmt::Return(Some(e))
        | Stmt::Throw(e)
        | Stmt::Var(greycat_analyzer_hir::types::LocalVar { init: Some(e), .. }) => {
            emit_call_arg_hints_expr(hir, resolutions, *e, want, text, out);
        }
        Stmt::Assign(a) => {
            emit_call_arg_hints_expr(hir, resolutions, a.target, want, text, out);
            emit_call_arg_hints_expr(hir, resolutions, a.value, want, text, out);
        }
        Stmt::If(i) => {
            emit_call_arg_hints_expr(hir, resolutions, i.condition, want, text, out);
            emit_call_arg_hints(hir, resolutions, i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_call_arg_hints(hir, resolutions, eb, want, text, out);
            }
        }
        Stmt::While(w) => {
            emit_call_arg_hints_expr(hir, resolutions, w.condition, want, text, out);
            emit_call_arg_hints(hir, resolutions, w.body, want, text, out);
        }
        Stmt::DoWhile(w) => {
            emit_call_arg_hints(hir, resolutions, w.body, want, text, out);
            emit_call_arg_hints_expr(hir, resolutions, w.condition, want, text, out);
        }
        Stmt::For(f) => emit_call_arg_hints(hir, resolutions, f.body, want, text, out),
        Stmt::ForIn(f) => {
            emit_call_arg_hints_expr(hir, resolutions, f.range, want, text, out);
            emit_call_arg_hints(hir, resolutions, f.body, want, text, out);
        }
        Stmt::Try(t) => {
            emit_call_arg_hints(hir, resolutions, t.try_block, want, text, out);
            emit_call_arg_hints(hir, resolutions, t.catch_block, want, text, out);
        }
        Stmt::At(a) => {
            emit_call_arg_hints_expr(hir, resolutions, a.expr, want, text, out);
            emit_call_arg_hints(hir, resolutions, a.block, want, text, out);
        }
        _ => {}
    }
}

fn emit_call_arg_hints_expr(
    hir: &Hir,
    resolutions: &greycat_analyzer_analysis::resolver::Resolutions,
    expr_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    use greycat_analyzer_hir::types::{CallExpr, Expr};
    match &hir.exprs[expr_id] {
        Expr::Call(CallExpr { callee, args, .. }) => {
            // Recurse into nested args first so hints fire on inner
            // calls too.
            emit_call_arg_hints_expr(hir, resolutions, *callee, want, text, out);
            for a in args {
                emit_call_arg_hints_expr(hir, resolutions, *a, want, text, out);
            }
            // Look up callee's params.
            if let Expr::Ident(name_idx) = &hir.exprs[*callee]
                && let Some(Definition::Decl(decl_id)) = resolutions.lookup(*name_idx)
                && let Decl::Fn(fnd) = &hir.decls[decl_id]
            {
                for (i, arg) in args.iter().enumerate() {
                    let Some(p_id) = fnd.params.get(i) else {
                        break;
                    };
                    let p = &hir.fn_params[*p_id];
                    let param_name = hir.idents[p.name].text.clone();
                    if param_name.starts_with('_') {
                        continue;
                    }
                    let arg_range = match &hir.exprs[*arg] {
                        Expr::Ident(ident_idx) => hir.idents[*ident_idx].byte_range.clone(),
                        other => other.byte_range(),
                    };
                    if arg_range.start > want.1 || arg_range.end < want.0 {
                        continue;
                    }
                    out.push(InlayHint {
                        position: byte_to_position(text, arg_range.start),
                        label: InlayHintLabel::String(format!("{param_name}:")),
                        kind: Some(InlayHintKind::PARAMETER),
                        text_edits: None,
                        tooltip: None,
                        padding_left: None,
                        padding_right: Some(true),
                        data: None,
                    });
                }
            }
        }
        Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for e in items {
                emit_call_arg_hints_expr(hir, resolutions, *e, want, text, out);
            }
        }
        Expr::Member(m) | Expr::Arrow(m) => {
            emit_call_arg_hints_expr(hir, resolutions, m.receiver, want, text, out);
        }
        Expr::Offset(o) => {
            emit_call_arg_hints_expr(hir, resolutions, o.receiver, want, text, out);
            emit_call_arg_hints_expr(hir, resolutions, o.index, want, text, out);
        }
        Expr::Binary(b) => {
            emit_call_arg_hints_expr(hir, resolutions, b.left, want, text, out);
            emit_call_arg_hints_expr(hir, resolutions, b.right, want, text, out);
        }
        Expr::Unary(u) => emit_call_arg_hints_expr(hir, resolutions, u.operand, want, text, out),
        Expr::Paren(inner, _) => {
            emit_call_arg_hints_expr(hir, resolutions, *inner, want, text, out)
        }
        Expr::Object(o) => {
            for f in &o.fields {
                emit_call_arg_hints_expr(hir, resolutions, f.value, want, text, out);
            }
        }
        Expr::Lambda(l) => emit_call_arg_hints_expr(hir, resolutions, l.body, want, text, out),
        Expr::Is { value, .. } | Expr::Cast { value, .. } => {
            emit_call_arg_hints_expr(hir, resolutions, *value, want, text, out);
        }
        _ => {}
    }
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

// =============================================================================
// P4.1 — formatting
// =============================================================================

/// Whole-document formatting. Returns a single `TextEdit` that replaces
/// the entire document range when the formatter's output differs from
/// the input. Returns `None` (no edits) when the document is already
/// formatted.
pub fn formatting(text: &str, root: tree_sitter::Node<'_>) -> Option<Vec<TextEdit>> {
    let formatted = greycat_analyzer_fmt::format_tree(text, root);
    if formatted == text {
        return Some(Vec::new());
    }
    let last_byte = text.len();
    let end_pos = byte_to_position(text, last_byte);
    Some(vec![TextEdit {
        range: lsp_types::Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: end_pos,
        },
        new_text: formatted,
    }])
}

/// P8.8 range formatting — format only the text inside `range`. The
/// foundational formatter (P4.1) operates on whole-tree input, so the
/// implementation snapshots the slice, formats it, and returns a single
/// replacement edit covering the requested range. Falls back to no
/// edits when the slice doesn't change.
pub fn range_formatting(
    text: &str,
    root: tree_sitter::Node<'_>,
    range: lsp_types::Range,
) -> Option<Vec<TextEdit>> {
    let _ = root;
    let start = position_to_byte(text, range.start);
    let end = position_to_byte(text, range.end);
    if end <= start || end > text.len() {
        return Some(Vec::new());
    }
    let slice = &text[start..end];
    let sub_tree = greycat_analyzer_syntax::parse(slice);
    let formatted = greycat_analyzer_fmt::format_tree(slice, sub_tree.root_node());
    if formatted == slice {
        return Some(Vec::new());
    }
    Some(vec![TextEdit {
        range,
        new_text: formatted,
    }])
}

/// P8.5 workspace symbols — aggregate every document's
/// [`document_symbols`] output, then flatten to `WorkspaceSymbol`s
/// keyed by URI. The `query` filter is a simple case-insensitive
/// substring match against the symbol name (TS reference does the
/// same).
pub fn workspace_symbols(
    docs: impl IntoIterator<Item = (Uri, String, String)>,
    query: &str,
) -> Vec<WorkspaceSymbol> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for (uri, lib, text) in docs {
        let tree = greycat_analyzer_syntax::parse(&text);
        let symbols = document_symbols(&text, &lib, tree.root_node());
        flatten_workspace(&uri, &symbols, &needle, &mut out);
    }
    out
}

fn flatten_workspace(
    uri: &Uri,
    symbols: &[DocumentSymbol],
    needle: &str,
    out: &mut Vec<WorkspaceSymbol>,
) {
    for sym in symbols {
        if needle.is_empty() || sym.name.to_lowercase().contains(needle) {
            out.push(WorkspaceSymbol {
                name: sym.name.clone(),
                kind: sym.kind,
                tags: sym.tags.clone(),
                container_name: None,
                location: OneOf::Left(Location {
                    uri: uri.clone(),
                    range: sym.selection_range,
                }),
                data: None,
            });
        }
        if let Some(children) = &sym.children {
            flatten_workspace(uri, children, needle, out);
        }
    }
}

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

// =============================================================================
// On-demand diagnostics for capabilities that don't sit on the publish path
// =============================================================================

/// Run the full pipeline (HIR lower → resolver → analyzer + lints) against
/// `text` and convert every finding to an `lsp_types::Diagnostic`. Used by
/// per-request capabilities like `code_actions` that need a fresh diagnostic
/// list without consulting the [`crate::backend::Backend`]'s cached
/// [`greycat_analyzer_analysis::project::ProjectAnalysis`].
pub(crate) fn current_diagnostics(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
) -> Vec<Diagnostic> {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let analysis = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);

    let mut out: Vec<Diagnostic> = analysis
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: byte_range_to_lsp_range(text, &d.byte_range),
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String("semantic".into())),
            source: Some("greycat-analyzer".into()),
            message: d.message.clone(),
            ..Default::default()
        })
        .collect();

    for lint in run_lints(&hir, &resolutions) {
        out.push(Diagnostic {
            range: byte_range_to_lsp_range(text, &lint.byte_range),
            severity: Some(match lint.severity {
                LintSeverity::Error => DiagnosticSeverity::ERROR,
                LintSeverity::Warning => DiagnosticSeverity::WARNING,
                LintSeverity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String(lint.rule.into())),
            source: Some("lint".into()),
            message: lint.message,
            ..Default::default()
        });
    }
    out
}

fn byte_range_to_lsp_range(text: &str, range: &std::ops::Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: byte_to_position(text, range.start),
        end: byte_to_position(text, range.end),
    }
}
