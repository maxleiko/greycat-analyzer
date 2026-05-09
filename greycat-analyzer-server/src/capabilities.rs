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
use greycat_analyzer_analysis::project::{ModuleAnalysis, ProjectAnalysis};
use greycat_analyzer_analysis::resolver::{Definition, resolve};
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, FnDecl, TypeDecl};
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
// P3.1 + P15.1 — hover
// =============================================================================

/// Hover info at `pos`. Three layers of lookup:
/// 1. Cursor on an `ident` node — surface the resolver `Definition`'s
///    binding info (param/local type, decl signature, builtin name).
/// 2. Cursor inside a non-ident HIR `Expr` — surface
///    `<short-label>: <inferred-type>`.
/// 3. No HIR shape covers the cursor — return `None`.
///
/// In-module hover only — for cross-module provenance and richer cross-
/// module signatures, callers thread a `ProjectAnalysis` + `SourceManager`
/// through [`hover_with_project`].
pub fn hover(text: &str, lib: &str, root: tree_sitter::Node<'_>, pos: Position) -> Option<Hover> {
    hover_inner(text, lib, root, pos)
}

/// P15.1 — hover with project context. Restores cross-module hover
/// content lost in earlier phases:
/// * doc-comments above the foreign decl,
/// * full function signature / type-decl shape,
/// * `defined in <module>` provenance footnote.
///
/// Consumes the cached `ModuleAnalysis` for `uri` directly (so cross-
/// module name resolution flows through the project index). Falls back
/// to the in-module-only [`hover`] when the cache is empty.
pub fn hover_with_project(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    manager: &SourceManager,
) -> Option<Hover> {
    if let Some(module) = project.module(uri) {
        let byte = position_to_byte(text, pos);
        let node = node_at_offset(root, byte)?;
        if !node.is_named() {
            return None;
        }
        // --- Layer 1: ident-based hover via cached resolutions.
        if node.kind() == "ident"
            && let Some((ident_idx, ident)) = module
                .hir
                .idents
                .iter()
                .find(|(_, i)| i.byte_range == node.byte_range())
        {
            if let Some(markdown) = ident_hover_markdown(
                &module.hir,
                &module.resolutions,
                &module.analysis,
                project.arena(),
                ident_idx,
                ident,
                Some(HoverProjectCtx { project, manager }),
            ) {
                return Some(hover_from_markdown(
                    markdown,
                    ident.byte_range.clone(),
                    text,
                ));
            }
            // Decl-defining ident.
            if let Some(m) = module.hir.module.as_ref() {
                for decl_id in &m.decls {
                    let decl = &module.hir.decls[*decl_id];
                    if let Some(name_id) = decl.name()
                        && module.hir.idents[name_id].byte_range == node.byte_range()
                    {
                        let markdown = render_decl_hover_markdown(&module.hir, decl, None);
                        return Some(hover_from_markdown(
                            markdown,
                            module.hir.idents[name_id].byte_range.clone(),
                            text,
                        ));
                    }
                }
            }
        }
        // --- Layer 2: non-ident expression hover (cached analysis).
        let target_range = node.byte_range();
        for ancestor in ancestors(node) {
            let r = ancestor.byte_range();
            if r.start > target_range.start || r.end < target_range.end {
                break;
            }
            if let Some((expr_id, expr)) = module
                .hir
                .exprs
                .iter()
                .filter(|(_, e)| !matches!(e, Expr::Ident(_)))
                .find(|(_, e)| {
                    let er = e.byte_range();
                    !er.is_empty() && er == r
                })
                && let Some(ty) = module.analysis.expr_types.get(&expr_id)
            {
                let label = format!(
                    "{}: {}",
                    short_expr_label(&module.hir, expr),
                    greycat_analyzer_types::display(project.arena(), *ty),
                );
                return Some(hover_from_markdown(wrap_code(&label), r, text));
            }
        }
        return None;
    }
    // Cache miss — fall back to in-module-only hover.
    hover_inner(text, lib, root, pos)
}

#[derive(Copy, Clone)]
struct HoverProjectCtx<'a> {
    project: &'a ProjectAnalysis,
    manager: &'a SourceManager,
}

fn hover_inner(text: &str, lib: &str, root: tree_sitter::Node<'_>, pos: Position) -> Option<Hover> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if !node.is_named() {
        return None;
    }

    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let (arena, analysis) = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);

    // --- Layer 1: ident-based hover (params / locals / decls / builtins).
    if node.kind() == "ident"
        && let Some((ident_idx, ident)) = hir
            .idents
            .iter()
            .find(|(_, i)| i.byte_range == node.byte_range())
    {
        if let Some(markdown) = ident_hover_markdown(
            &hir,
            &resolutions,
            &analysis,
            &arena,
            ident_idx,
            ident,
            None,
        ) {
            return Some(hover_from_markdown(
                markdown,
                ident.byte_range.clone(),
                text,
            ));
        }
        // Decl-defining ident (e.g. cursor on the `helper` in `fn helper()`).
        if let Some(module) = hir.module.as_ref() {
            for decl_id in &module.decls {
                let decl = &hir.decls[*decl_id];
                if let Some(name_id) = decl.name()
                    && hir.idents[name_id].byte_range == node.byte_range()
                {
                    let markdown = render_decl_hover_markdown(&hir, decl, None);
                    return Some(hover_from_markdown(
                        markdown,
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
                greycat_analyzer_types::display(&arena, *ty),
            );
            return Some(hover_from_markdown(wrap_code(&label), r, text));
        }
    }

    None
}

fn ident_hover_markdown(
    hir: &Hir,
    resolutions: &greycat_analyzer_analysis::resolver::Resolutions,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    arena: &greycat_analyzer_types::TypeArena,
    ident_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
    ident: &greycat_analyzer_hir::types::Ident,
    project: Option<HoverProjectCtx<'_>>,
) -> Option<String> {
    // P6.3: property idents in `a.b` aren't in `Resolutions` — they
    // bind to a `TypeAttr` / method via the analyzer's member pass.
    // Check that first so member hovers render with the right shape.
    if let Some(member) = analysis.member_lookup(ident_idx) {
        return Some(member_hover_markdown(hir, member, ident));
    }
    // P11.5 — cross-module member binding (`a.b` where the receiver
    // type lives in another module, or `Type::method` where Type is
    // declared cross-module). The foreign decl's HIR lives in another
    // module, so we render its signature there.
    if let Some(ctx) = project
        && let Some(foreign) = analysis.foreign_member_lookup(ident_idx)
        && let Some(fmod) = ctx.project.module(&foreign.uri)
    {
        let provenance = module_label_for_uri(&foreign.uri);
        return Some(foreign_member_hover_markdown(
            &fmod.hir,
            &foreign.member,
            ident,
            &provenance,
        ));
    }
    // P15.x — chain-segment foreign-decl binding (e.g. `Identity` in
    // `runtime::Identity::create`). Renders the foreign type/fn/enum
    // decl with a `defined in <module>` footnote.
    if let Some(ctx) = project
        && let Some(fdecl) = analysis.foreign_decl_lookup(ident_idx)
        && let Some(fmod) = ctx.project.module(&fdecl.uri)
        && (fdecl.decl.into_raw() as usize) < fmod.hir.decls.len()
    {
        let provenance = module_label_for_uri(&fdecl.uri);
        return Some(render_decl_hover_markdown(
            &fmod.hir,
            &fmod.hir.decls[fdecl.decl],
            Some(&provenance),
        ));
    }
    match resolutions.lookup(ident_idx)? {
        Definition::Param(name) | Definition::Local(name) => {
            analysis.def_types.get(&name).map(|ty| {
                wrap_code(&format!(
                    "{}: {}",
                    ident.text,
                    greycat_analyzer_types::display(arena, *ty),
                ))
            })
        }
        Definition::Decl(decl_id) => {
            Some(render_decl_hover_markdown(hir, &hir.decls[decl_id], None))
        }
        Definition::Generic(_) => Some(wrap_code(&format!("(type parameter) {}", ident.text))),
        Definition::ProjectDecl {
            uri: foreign_uri,
            decl,
        } => {
            // P15.1 — try to render the foreign decl's full signature
            // + doc + provenance footnote when project context is
            // available. Falls back to a minimal placeholder otherwise.
            if let Some(ctx) = project
                && let Some(fmod) = ctx.project.module(&foreign_uri)
                && (decl.into_raw() as usize) < fmod.hir.decls.len()
            {
                let provenance = module_label_for_uri(&foreign_uri);
                return Some(render_decl_hover_markdown(
                    &fmod.hir,
                    &fmod.hir.decls[decl],
                    Some(&provenance),
                ));
            }
            Some(wrap_code(&format!("(project) {}", ident.text)))
        }
        Definition::Project => Some(wrap_code(&format!("(runtime built-in) {}", ident.text))),
    }
}

/// Hover markdown for a property ident bound by P6.3 member resolution
/// (`a.b` / `a->b`). Renders attribute / method shape with the
/// declared / inferred return type when available.
fn member_hover_markdown(
    hir: &Hir,
    member: greycat_analyzer_analysis::analyzer::MemberDef,
    ident: &greycat_analyzer_hir::types::Ident,
) -> String {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    match member {
        MemberDef::Attr(attr_id) => {
            let attr = &hir.type_attrs[attr_id];
            let ty_str = attr
                .ty
                .map(|t| render_type_ref(hir, t))
                .unwrap_or_else(|| "any".into());
            let mut out = String::new();
            push_doc_section(&mut out, attr.doc.as_deref());
            out.push_str(&wrap_code(&format!("{}: {}", ident.text, ty_str)));
            out
        }
        MemberDef::Method(decl_id) => {
            let decl = &hir.decls[decl_id];
            render_decl_hover_markdown(hir, decl, None)
        }
    }
}

/// P15.x — cross-module variant of [`member_hover_markdown`]. Reads
/// the foreign HIR for the attr / method and appends an italic
/// `*defined in `<module>`*` footnote.
fn foreign_member_hover_markdown(
    foreign_hir: &Hir,
    member: &greycat_analyzer_analysis::analyzer::MemberDef,
    ident: &greycat_analyzer_hir::types::Ident,
    provenance: &str,
) -> String {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    let mut out = match member {
        MemberDef::Attr(attr_id) => {
            let attr = &foreign_hir.type_attrs[*attr_id];
            let ty_str = attr
                .ty
                .map(|t| render_type_ref(foreign_hir, t))
                .unwrap_or_else(|| "any".into());
            let mut s = String::new();
            push_doc_section(&mut s, attr.doc.as_deref());
            s.push_str(&wrap_code(&format!("{}: {}", ident.text, ty_str)));
            s
        }
        MemberDef::Method(decl_id) => {
            let decl = &foreign_hir.decls[*decl_id];
            render_decl_hover_markdown(foreign_hir, decl, None)
        }
    };
    out.push_str("\n\n*defined in `");
    out.push_str(provenance);
    out.push_str("`*");
    out
}

/// P15.1 — render a top-level decl as hover markdown. Output layout:
/// optional doc paragraph, then a ```greycat fenced code block with the
/// signature, then (when `provenance` is `Some`) an italic
/// "*defined in `<name>`*" footnote. `provenance` is supplied only for
/// cross-module idents — intra-module uses pass `None`.
fn render_decl_hover_markdown(hir: &Hir, decl: &Decl, provenance: Option<&str>) -> String {
    let mut out = String::new();
    push_doc_section(&mut out, decl_doc(decl));
    let signature = render_decl_signature(hir, decl);
    out.push_str(&wrap_code(&signature));
    if let Some(prov) = provenance {
        out.push_str("\n\n*defined in `");
        out.push_str(prov);
        out.push_str("`*");
    }
    out
}

fn decl_doc(decl: &Decl) -> Option<&str> {
    match decl {
        Decl::Fn(d) => d.doc.as_deref(),
        Decl::Type(d) => d.doc.as_deref(),
        Decl::Enum(d) => d.doc.as_deref(),
        Decl::Var(_) => None,
        Decl::Pragma(_) => None,
    }
}

/// Render a decl as a single-line code-block-friendly signature.
/// `fn` decls render the full `fn name<G>(p: T): R`; types render
/// `type Name<G> extends Parent`; enums render `enum Name`; vars
/// render `var name: T`.
fn render_decl_signature(hir: &Hir, decl: &Decl) -> String {
    match decl {
        Decl::Fn(d) => render_fn_signature(hir, d),
        Decl::Type(d) => render_type_signature(hir, d),
        Decl::Enum(d) => format!("enum {}", hir.idents[d.name].text),
        Decl::Var(d) => {
            let ty =
                d.ty.map(|t| render_type_ref(hir, t))
                    .unwrap_or_else(|| "any".into());
            format!("var {}: {}", hir.idents[d.name].text, ty)
        }
        Decl::Pragma(p) => format!("@{}", hir.idents[p.name].text),
    }
}

fn render_fn_signature(hir: &Hir, fnd: &FnDecl) -> String {
    let name = &hir.idents[fnd.name].text;
    let mut out = String::new();
    if fnd.modifiers.private {
        out.push_str("private ");
    }
    if fnd.modifiers.static_ {
        out.push_str("static ");
    }
    if fnd.modifiers.abstract_ {
        out.push_str("abstract ");
    }
    if fnd.modifiers.native {
        out.push_str("native ");
    }
    out.push_str("fn ");
    out.push_str(name);
    if !fnd.generics.is_empty() {
        out.push('<');
        for (i, g) in fnd.generics.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&hir.idents[*g].text);
        }
        out.push('>');
    }
    out.push('(');
    for (i, param_id) in fnd.params.iter().enumerate() {
        let p = &hir.fn_params[*param_id];
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&hir.idents[p.name].text);
        out.push_str(": ");
        match p.ty {
            Some(t) => out.push_str(&render_type_ref(hir, t)),
            None => out.push_str("any"),
        }
    }
    out.push(')');
    if let Some(ret) = fnd.return_type {
        out.push_str(": ");
        out.push_str(&render_type_ref(hir, ret));
    }
    out
}

fn render_type_signature(hir: &Hir, td: &TypeDecl) -> String {
    let mut out = String::new();
    if td.modifiers.private {
        out.push_str("private ");
    }
    if td.modifiers.abstract_ {
        out.push_str("abstract ");
    }
    if td.modifiers.native {
        out.push_str("native ");
    }
    out.push_str("type ");
    out.push_str(&hir.idents[td.name].text);
    if !td.generics.is_empty() {
        out.push('<');
        for (i, g) in td.generics.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&hir.idents[*g].text);
        }
        out.push('>');
    }
    if let Some(parent) = td.supertype {
        out.push_str(" extends ");
        out.push_str(&render_type_ref(hir, parent));
    }
    out
}

fn render_type_ref(
    hir: &Hir,
    type_ref: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeRef>,
) -> String {
    let tr = &hir.type_refs[type_ref];
    let mut out = hir.idents[tr.name].text.clone();
    if !tr.params.is_empty() {
        out.push('<');
        for (i, p) in tr.params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&render_type_ref(hir, *p));
        }
        out.push('>');
    }
    if tr.optional {
        out.push('?');
    }
    out
}

/// Best-effort module label for a foreign URI. Strips trailing `.gcl`
/// off the file name so `file:///proj/lib/std/core.gcl` renders as
/// `core` in the provenance footnote. Falls back to the URI string
/// when path parsing fails.
fn module_label_for_uri(uri: &Uri) -> String {
    let s = uri.as_str();
    let path_part = s.strip_prefix("file://").unwrap_or(s);
    let last = path_part.rsplit(['/', '\\']).next().unwrap_or(path_part);
    last.strip_suffix(".gcl").unwrap_or(last).to_string()
}

fn push_doc_section(out: &mut String, doc: Option<&str>) {
    let Some(doc) = doc else { return };
    let trimmed = doc.trim();
    if trimmed.is_empty() {
        return;
    }
    out.push_str(trimmed);
    out.push_str("\n\n");
}

fn wrap_code(label: &str) -> String {
    format!("```greycat\n{label}\n```")
}

fn hover_from_markdown(markdown: String, range: Range<usize>, text: &str) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
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
    let (_arena, analysis) = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);
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

/// P15.9 — goto-def on a module-name segment of a `static_expr` chain.
/// In `runtime::Identity::create`, the leftmost ident `runtime` names
/// the module that owns `Identity`. This helper checks whether the
/// cursor sits on the leftmost segment of such a chain and, if so,
/// returns the URI of the matching `.gcl` file (jumping to its first
/// line). Returns `None` otherwise — caller falls through to the
/// regular goto-def flow.
pub fn goto_module_segment(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    manager: &SourceManager,
) -> Option<Location> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if node.kind() != "ident" {
        return None;
    }
    // The leftmost `type_ident` in a `static_expr` chain is the
    // module-name slot. Walk up to confirm the parent shape.
    let parent = node.parent()?;
    if parent.kind() != "type_ident" {
        return None;
    }
    let static_parent = parent.parent()?;
    if static_parent.kind() != "static_expr" {
        return None;
    }
    let cursor_text = text.get(node.byte_range())?.to_string();
    // Match against any cached doc whose `name()` matches the cursor
    // text. `Document::name()` is the filename without `.gcl`, which
    // is the convention GreyCat's `runtime::X` chains rely on.
    for (uri, cell) in manager.iter() {
        let doc = cell.borrow();
        if doc.name() == cursor_text {
            return Some(Location {
                uri: uri.clone(),
                range: lsp_types::Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
            });
        }
    }
    None
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

/// Project-aware variant — reads the cached diagnostics + lints from
/// the [`ProjectAnalysis`] entry for `uri` instead of re-running the
/// whole pipeline. Same convention as the rest of the
/// `*_with_project` family: the LSP server handler in
/// [`crate::server`] always goes through this path so the cross-
/// module fixup passes (P15.7 / P16.3 / P16.4) feed into the
/// diagnostic list.
pub fn code_actions_with_project(
    module: &ModuleAnalysis,
    text: &str,
    uri: &Uri,
    range: lsp_types::Range,
) -> Vec<CodeActionOrCommand> {
    // Code actions don't differentiate lib vs project — the user's
    // already pointing at a specific diagnostic when invoking them.
    let semantic = diagnostics_from_module(text, module, true);
    code_actions_from_diagnostics(text, uri, range, semantic)
}

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
    code_actions_from_diagnostics(text, uri, range, semantic)
}

fn code_actions_from_diagnostics(
    text: &str,
    uri: &Uri,
    range: lsp_types::Range,
    semantic: Vec<Diagnostic>,
) -> Vec<CodeActionOrCommand> {
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
        "possibly-null" => {
            // Lint range = the receiver. Find the access operator that
            // follows (`.`, `->`, `[`) and insert `?` immediately
            // before it, mirroring the TS reference's "fix available".
            let recv_end = position_to_byte(text, diag.range.end);
            let Some(op_pos) = find_access_op_after(text, recv_end) else {
                return Vec::new();
            };
            // Already null-safe — no fix to offer (defensive guard;
            // the lint shouldn't have fired in the first place).
            if text.as_bytes().get(op_pos) == Some(&b'?') {
                return Vec::new();
            }
            let pos = byte_to_position(text, op_pos);
            vec![TextEdit {
                range: lsp_types::Range {
                    start: pos,
                    end: pos,
                },
                new_text: "?".into(),
            }]
        }
        "redundant-nullable-access" => {
            // Lint range covers the operator slice (`?.` / `?->` / `?[`
            // plus any whitespace). Drop the lone `?` byte inside it.
            let start = position_to_byte(text, diag.range.start);
            let end = position_to_byte(text, diag.range.end);
            if end <= start || end > text.len() {
                return Vec::new();
            }
            let bytes = text.as_bytes();
            let Some(q) = bytes[start..end]
                .iter()
                .position(|b| *b == b'?')
                .map(|off| start + off)
            else {
                return Vec::new();
            };
            vec![TextEdit {
                range: lsp_types::Range {
                    start: byte_to_position(text, q),
                    end: byte_to_position(text, q + 1),
                },
                new_text: String::new(),
            }]
        }
        "redundant-non-null-assertion" | "redundant-coalesce" => {
            // Lint range = the dead-weight slice (`!!` for the unary,
            // `?? rhs` for the coalesce). Delete it.
            vec![TextEdit {
                range: diag.range,
                new_text: String::new(),
            }]
        }
        "modvar-node-cannot-be-nullable" => {
            // Lint range = the type ref (e.g. `node<float?>?`). Drop the
            // trailing `?` byte.
            let end = position_to_byte(text, diag.range.end);
            if end == 0 || text.as_bytes().get(end - 1) != Some(&b'?') {
                return Vec::new();
            }
            vec![TextEdit {
                range: lsp_types::Range {
                    start: byte_to_position(text, end - 1),
                    end: byte_to_position(text, end),
                },
                new_text: String::new(),
            }]
        }
        "modvar-node-inner-must-be-nullable" => {
            // Lint range = the inner type ref (e.g. `int` in `node<int>`).
            // Append `?` at its end.
            vec![TextEdit {
                range: lsp_types::Range {
                    start: diag.range.end,
                    end: diag.range.end,
                },
                new_text: "?".into(),
            }]
        }
        _ => Vec::new(),
    }
}

/// Scan forward from `from` over whitespace until we hit an access
/// operator (`.`, `->`, `[`, or an existing `?`). Returns the operator's
/// starting byte index, or `None` if the next non-whitespace byte isn't
/// one of those.
fn find_access_op_after(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'.' | b'[' | b'?' => return Some(i),
            b'-' if bytes.get(i + 1) == Some(&b'>') => return Some(i),
            _ => return None,
        }
    }
    None
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

/// LSP entry point for inlay hints — consumes the cached
/// [`ModuleAnalysis`] from [`ProjectAnalysis`] so the cross-module
/// fixup passes (P16.3 cross-module member typing, P16.4 call-on-
/// member return-type inference, P15.7 cross-module call return-type
/// inference) all flow through. Capabilities that re-run a single-
/// file [`analyzer::analyze`] would miss those — that's the bug we
/// kept hitting whenever new project-level inference landed.
///
/// Convention: every LSP handler in [`crate::server`] reads from
/// `Backend::project_analysis` and calls one of these `*_with_project`
/// variants. The legacy `(text, lib, root)` shims below stay for
/// unit tests / single-file CLI commands but they must never be
/// reached from a live LSP session.
pub fn inlay_hints_with_project(
    module: &ModuleAnalysis,
    arena: &greycat_analyzer_types::TypeArena,
    text: &str,
    range: &lsp_types::Range,
) -> Vec<InlayHint> {
    let hir = &module.hir;
    let resolutions = &module.resolutions;
    let analysis = &module.analysis;

    let hir_module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let want = (
        position_to_byte(text, range.start),
        position_to_byte(text, range.end),
    );

    let mut out = Vec::new();
    for decl_id in &hir_module.decls {
        if let Decl::Fn(fnd) = &hir.decls[*decl_id] {
            // P13.7: return-type hint when the fn has no declared
            // return type but the analyzer inferred one from the body.
            if fnd.return_type.is_none()
                && let Some(body) = fnd.body
                && let Some(ty) = inferred_fn_return(hir, analysis, body)
            {
                let name_range = &hir.idents[fnd.name].byte_range;
                if name_range.start <= want.1 && name_range.end >= want.0 {
                    let label = format!(": {}", greycat_analyzer_types::display(arena, ty));
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
                emit_var_hints(hir, analysis, arena, body, want, text, &mut out);
                // P13.7: argument-name hints inside the body.
                emit_call_arg_hints(hir, resolutions, body, want, text, &mut out);
            }
        }
    }
    out
}

/// Single-file shim — only used by unit tests / single-file CLI
/// commands. **The LSP server never calls this directly**; it goes
/// through [`inlay_hints_with_project`] so the cross-module fixup
/// passes apply. Marked `#[doc(hidden)]` to keep external consumers
/// pointed at the project-aware path.
#[doc(hidden)]
pub fn inlay_hints(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    range: &lsp_types::Range,
) -> Vec<InlayHint> {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let (arena, analysis) = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);
    let module = ModuleAnalysis {
        hir,
        resolutions,
        analysis,
        lints: Vec::new(),
        lib: lib.to_string(),
        timings: Default::default(),
    };
    inlay_hints_with_project(&module, &arena, text, range)
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
    let block = match &hir.stmts[body] {
        Stmt::Block(b) => b,
        _ => return None,
    };
    for s in block.stmts.iter().rev() {
        if let Stmt::Return(Some(e)) = &hir.stmts[*s] {
            return analysis.expr_types.get(e).copied();
        }
    }
    None
}

/// Same as [`emit_call_arg_hints`] but recurses into a `BlockStmt`
/// directly, since body-bearing fields (`If::then_branch`, …) hold
/// the block inline now.
fn emit_call_arg_hints_block(
    hir: &Hir,
    resolutions: &greycat_analyzer_analysis::resolver::Resolutions,
    block: &greycat_analyzer_hir::types::BlockStmt,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_call_arg_hints(hir, resolutions, *s, want, text, out);
    }
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
        Stmt::Block(b) => emit_call_arg_hints_block(hir, resolutions, b, want, text, out),
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
            emit_call_arg_hints_block(hir, resolutions, &i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_call_arg_hints(hir, resolutions, eb, want, text, out);
            }
        }
        Stmt::While(w) => {
            emit_call_arg_hints_expr(hir, resolutions, w.condition, want, text, out);
            emit_call_arg_hints_block(hir, resolutions, &w.body, want, text, out);
        }
        Stmt::DoWhile(w) => {
            emit_call_arg_hints_block(hir, resolutions, &w.body, want, text, out);
            emit_call_arg_hints_expr(hir, resolutions, w.condition, want, text, out);
        }
        Stmt::For(f) => emit_call_arg_hints_block(hir, resolutions, &f.body, want, text, out),
        Stmt::ForIn(f) => {
            emit_call_arg_hints_expr(hir, resolutions, f.range, want, text, out);
            emit_call_arg_hints_block(hir, resolutions, &f.body, want, text, out);
        }
        Stmt::Try(t) => {
            emit_call_arg_hints_block(hir, resolutions, &t.try_block, want, text, out);
            emit_call_arg_hints_block(hir, resolutions, &t.catch_block, want, text, out);
        }
        Stmt::At(a) => {
            emit_call_arg_hints_expr(hir, resolutions, a.expr, want, text, out);
            emit_call_arg_hints_block(hir, resolutions, &a.block, want, text, out);
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

/// Walk a `BlockStmt` recursively for var-hint emission. Body-bearing
/// statements hold the block inline post-refactor so we can't go via
/// `Idx<Stmt>` for them.
fn emit_var_hints_block(
    hir: &Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    arena: &greycat_analyzer_types::TypeArena,
    block: &greycat_analyzer_hir::types::BlockStmt,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_var_hints(hir, analysis, arena, *s, want, text, out);
    }
}

fn emit_var_hints(
    hir: &Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    arena: &greycat_analyzer_types::TypeArena,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    use greycat_analyzer_hir::types::Stmt;
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => emit_var_hints_block(hir, analysis, arena, b, want, text, out),
        Stmt::Var(v) if v.ty.is_none() && v.init.is_some() => {
            let r = &v.byte_range;
            if r.end < want.0 || r.start > want.1 {
                return;
            }
            let init_id = v.init.unwrap();
            let Some(ty) = analysis.expr_types.get(&init_id).copied() else {
                return;
            };
            let label = format!(": {}", greycat_analyzer_types::display(arena, ty));
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
            emit_var_hints_block(hir, analysis, arena, &i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_var_hints(hir, analysis, arena, eb, want, text, out);
            }
        }
        Stmt::While(w) => emit_var_hints_block(hir, analysis, arena, &w.body, want, text, out),
        Stmt::DoWhile(w) => emit_var_hints_block(hir, analysis, arena, &w.body, want, text, out),
        Stmt::For(f) => emit_var_hints_block(hir, analysis, arena, &f.body, want, text, out),
        Stmt::ForIn(f) => emit_var_hints_block(hir, analysis, arena, &f.body, want, text, out),
        Stmt::Try(t) => {
            emit_var_hints_block(hir, analysis, arena, &t.try_block, want, text, out);
            emit_var_hints_block(hir, analysis, arena, &t.catch_block, want, text, out);
        }
        Stmt::At(a) => emit_var_hints_block(hir, analysis, arena, &a.block, want, text, out),
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
// P15.4 — completion (foundational; only @include path completion today)
// =============================================================================

/// LSP `textDocument/completion`. Foundational entry — today only handles
/// `@include("<cursor>")` directory completion (P15.4). Future chunks
/// extend this to scope-aware ident completion (P15.2), member
/// completion after `.` / `->`, and `@library` version completion via
/// the GreyCat registry (P15.3).
///
/// Returns `None` (LSP-side: empty list) when the cursor isn't in a
/// shape we know how to complete yet.
pub fn completion(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    project_root: Option<&std::path::Path>,
) -> Option<CompletionList> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if let Some(items) = include_dir_completion(text, node, byte, project_root) {
        return Some(CompletionList {
            is_incomplete: false,
            items,
        });
    }
    if let Some(items) = pragma_completion(text, byte) {
        return Some(CompletionList {
            is_incomplete: false,
            items,
        });
    }
    if let Some(items) = keyword_completion(text, node, byte) {
        return Some(CompletionList {
            is_incomplete: false,
            items,
        });
    }
    None
}

/// P15.2.3 — completion with project context. Same dispatcher chain as
/// [`completion`], but the ident-position branch enumerates scope-
/// visible names (locals / params / generics / in-module decls) plus
/// the cross-module project surface (`ProjectIndex::values` /
/// `decl_locations` / `BUILTIN_RUNTIME_TYPES` / primitives) alongside
/// the keyword list. Typed prefix filters all of them.
pub fn completion_with_project(
    text: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    project_root: Option<&std::path::Path>,
) -> Option<CompletionList> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    let mut items = if let Some(items) = include_dir_completion(text, node, byte, project_root) {
        items
    } else if let Some(items) = pragma_completion(text, byte) {
        items
    } else if let Some(items) = member_completion(text, root, byte, uri, project) {
        items
    } else if let Some(items) = static_completion(text, byte, project) {
        items
    } else if let Some(items) = type_position_completion(text, node, byte, uri, project) {
        items
    } else if let Some(items) = object_field_completion(text, node, byte, uri, project) {
        items
    } else {
        ident_or_keyword_completion(text, node, byte, uri, project)?
    };
    apply_call_paren_snippet(&mut items, text, byte);
    Some(CompletionList {
        is_incomplete: false,
        items,
    })
}

/// P15.4 — `@include("<cursor>")` directory completion. Activated when
/// the cursor sits inside a `string` (or its `string_fragment` child)
/// whose enclosing `mod_pragma`'s annotation name is `include`. Walks
/// the project root directly (a one-level `read_dir`, no recursion)
/// and returns each subdirectory as a `CompletionItem`. Case-insensitive
/// prefix-matches the cursor's already-typed text.
fn include_dir_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    project_root: Option<&std::path::Path>,
) -> Option<Vec<CompletionItem>> {
    let project_root = project_root?;
    // Walk up to find the enclosing `string` node, then confirm the
    // chain is `string -> args -> annotation` with annotation name
    // `include`.
    let string_node = ancestor_with_kind(node, "string")?;
    let args_node = ancestor_with_kind(string_node, "args")?;
    let annotation_node = ancestor_with_kind(args_node, "annotation")?;
    let mut name_cursor = annotation_node.walk();
    let name_text = annotation_node
        .named_children(&mut name_cursor)
        .find(|c| c.kind() == "ident")
        .and_then(|c| text.get(c.byte_range()))?;
    if name_text != "include" {
        return None;
    }
    let mod_pragma = ancestor_with_kind(annotation_node, "mod_pragma")?;
    let _ = mod_pragma; // confirm we're inside a top-level pragma

    // Read what the user has typed so far (the prefix between `"` and
    // the cursor). The string node's text range is the whole `"..."`;
    // the inner `string_fragment` child holds the unescaped content.
    let typed = string_prefix_at_cursor(text, string_node, cursor_byte);
    // Split on `/`: everything up to the last `/` is the directory
    // path the user is drilling into; the part after is the prefix
    // for the next completion list. Examples:
    //   ""             -> base = project_root, prefix = ""
    //   "src"          -> base = project_root, prefix = "src"
    //   "src/"         -> base = project_root/src, prefix = ""
    //   "src/util"     -> base = project_root/src, prefix = "util"
    let (rel_dir, prefix) = match typed.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), typed.clone()),
    };
    let mut base = project_root.to_path_buf();
    if !rel_dir.is_empty() {
        for seg in rel_dir.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            // Reject `..` to keep completion anchored under project_root.
            if seg == ".." {
                return Some(Vec::new());
            }
            base.push(seg);
        }
    }
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Some(Vec::new()),
    };
    let mut items = Vec::new();
    let prefix_lower = prefix.to_lowercase();
    // Conventional ignores apply at the project root only — a user
    // explicitly drilling into `lib/` or `target/` should still see
    // what's there.
    let at_root = rel_dir.is_empty();
    let skip: &[&str] = if at_root {
        &[
            "node_modules",
            "gcdata",
            ".git",
            "target",
            "lib",
            "bin",
            "files",
            "webroot",
        ]
    } else {
        &["node_modules", ".git"]
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if skip.contains(&name_str) || name_str.starts_with('.') {
            continue;
        }
        if !prefix_lower.is_empty() && !name_str.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: name_str.to_string(),
            kind: Some(CompletionItemKind::FOLDER),
            detail: Some("@include directory".into()),
            insert_text: Some(name_str.to_string()),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

fn ancestor_with_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cur = node;
    loop {
        if cur.kind() == kind {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

/// Read the text inside a `string` node from its opening quote up to
/// `cursor_byte`. Used for prefix-matching against the typed text
/// before the cursor.
fn string_prefix_at_cursor(
    text: &str,
    string_node: tree_sitter::Node<'_>,
    cursor_byte: usize,
) -> String {
    let r = string_node.byte_range();
    let raw = text.get(r.clone()).unwrap_or("");
    // Strip the leading quote(s).
    let opener_len = if raw.starts_with('"') { 1 } else { 0 };
    let content_start = r.start + opener_len;
    if cursor_byte <= content_start {
        return String::new();
    }
    let upto = cursor_byte.min(r.end);
    text.get(content_start..upto).unwrap_or("").to_string()
}

// =============================================================================
// P15.2.1 — pragma completion after `@`
// =============================================================================

/// Emit pragma completion items when the cursor sits right after a `@`
/// or partway through an annotation name (`@li|brary`). Mirrors the TS
/// reference's `PRAGMA_COMPLETION_ITEMS` set
/// (`packages/lang/src/project/analysis_result.ts:2537`). Returns `None`
/// when the cursor isn't in an annotation-start position so the parent
/// dispatcher can fall through to other completion shapes.
fn pragma_completion(text: &str, cursor_byte: usize) -> Option<Vec<CompletionItem>> {
    let typed = pragma_prefix_at_cursor(text, cursor_byte)?;
    let prefix_lower = typed.to_lowercase();
    let mut items = pragma_items()
        .into_iter()
        .filter(|item| {
            // Strip the leading `@` from the label before prefix-matching.
            let name = item.label.trim_start_matches('@');
            prefix_lower.is_empty() || name.to_lowercase().starts_with(&prefix_lower)
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Walk back from `cursor_byte` over `[A-Za-z0-9_]*` and check that the
/// preceding byte is `@`. Returns the typed prefix between `@` and the
/// cursor (empty string when the user just hit `@`). `None` when there's
/// no `@` or the run isn't word-shaped.
fn pragma_prefix_at_cursor(text: &str, cursor_byte: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i == 0 || bytes[i - 1] != b'@' {
        return None;
    }
    Some(text.get(i..cap).unwrap_or("").to_string())
}

/// The pragma list. Snippet bodies match the TS reference shape so
/// editors that honor `InsertTextFormat::Snippet` get tabstop-driven
/// completions for the parametric forms (`@library`, `@include`,
/// `@role`, `@permission`). Bare forms (`@expose`, `@volatile`) skip
/// the snippet format flag.
fn pragma_items() -> Vec<CompletionItem> {
    vec![
        CompletionItem {
            label: "@library".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("library(\"$1\", \"$2\");$0".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("Adds a library to the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@include".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("include(\"$1\");$0".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("Adds a source directory to the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@role".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("role(\"$1\", \"$2\");$0".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some("Defines a role for the project".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@permission".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("permission(\"$1\")$0".into()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            detail: Some(
                "Defines a permission for the project, or give a permission to a function".into(),
            ),
            ..Default::default()
        },
        CompletionItem {
            label: "@expose".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("expose".into()),
            detail: Some("Registers the function as an http endpoint".into()),
            ..Default::default()
        },
        CompletionItem {
            label: "@volatile".into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("volatile".into()),
            detail: Some(
                "Volatile types cannot be stored in graph and have loose upgrade rules".into(),
            ),
            ..Default::default()
        },
    ]
}

// =============================================================================
// P15.2.2 — keyword completion at statement / expression positions
// =============================================================================

/// Emit keyword completion items when the cursor sits at a statement
/// or expression position (not after `.` / `->` / `::` / `@`, not
/// inside a string / comment / type-ident / annotation). Filters by
/// the alphabetic prefix the user has already typed.
///
/// Type-position only emits the type keywords (`null` / type names);
/// since dedicated type completion (P15.2.6) hasn't landed yet, we
/// just bail when the cursor sits inside a `type_ident` so we don't
/// pollute that slot with statement keywords.
fn keyword_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
) -> Option<Vec<CompletionItem>> {
    if !is_keyword_position(text, node, cursor_byte) {
        return None;
    }
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let mut items: Vec<CompletionItem> = ALL_KEYWORDS
        .iter()
        .filter(|kw| prefix_lower.is_empty() || kw.starts_with(&prefix_lower))
        .map(|kw| CompletionItem {
            label: (*kw).into(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some((*kw).into()),
            ..Default::default()
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// `true` when the cursor is at a position where bare keywords make
/// sense — i.e. not in a member/static/ref-access RHS, not inside an
/// annotation (pragma completion handles that), not inside a string /
/// comment, and not in a type-ident slot.
fn is_keyword_position(text: &str, node: tree_sitter::Node<'_>, cursor_byte: usize) -> bool {
    // Skip strings, comments, doc-comments. These ancestors short-
    // circuit completely — completion has no business firing inside
    // them at this layer.
    for kind in [
        "string",
        "_string_fragment",
        "line_comment",
        "_block_comment",
        "doc_comment",
    ] {
        if ancestor_with_kind(node, kind).is_some() {
            return false;
        }
    }
    // Annotation context is owned by `pragma_completion` (P15.2.1).
    if ancestor_with_kind(node, "annotation").is_some() {
        return false;
    }
    // Type-position is owned by P15.2.6 — defer instead of polluting.
    if ancestor_with_kind(node, "type_ident").is_some() {
        return false;
    }
    // Walk back from cursor over the typed prefix and inspect the
    // separator byte. `.` / `:` / `>` / `@` mean we're on the RHS of
    // a member / static / ref / annotation chain.
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i > 0 {
        let sep = bytes[i - 1];
        if matches!(sep, b'.' | b':' | b'>' | b'@') {
            return false;
        }
    }
    true
}

/// Walk back from `cursor_byte` over `[A-Za-z0-9_]*` and return the
/// typed run as an owned string. Used to prefix-filter keyword and
/// (later) ident completion.
fn ident_prefix_at_cursor(text: &str, cursor_byte: usize) -> String {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    text.get(i..cap).unwrap_or("").to_string()
}

/// `true` iff `s` matches the grammar's `ident` shape
/// (`[A-Za-z_][A-Za-z0-9_]*`). Names that fail this — e.g. enum
/// variants declared as `"Africa/Abidjan"` — must be re-quoted
/// when the completion machinery surfaces them at a `Type::|`
/// insertion site.
fn is_ident_like(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Build a [`CompletionItem`] for an enum variant. `name` is the
/// variant's HIR-stored spelling (always unquoted, even for
/// string-named variants — quotes are stripped at lowering time so
/// `member_uses` matches against the chain's property text).
///
/// Spelling rules at the insertion site:
///   - If the cursor is inside a quoted property (`Foo::"Tim|"`),
///     the opening `"` is already in the buffer; emit the bare
///     variant text.
///   - Otherwise, emit ident-shaped names bare (`Foo::alpha`) and
///     escape + wrap non-ident names so the result is valid syntax
///     (`Foo::"Africa/Abidjan"`).
///
/// `replace_range` is the LSP range covering the surrounding word
/// at the cursor. Threading it through `text_edit` is what makes
/// "ask for completion mid-word" honest — the accepted text
/// replaces the existing word instead of doubling it via a naive
/// `insert_text` insertion at the cursor.
fn enum_variant_completion_item(
    name: &str,
    in_string: bool,
    replace_range: lsp_types::Range,
) -> CompletionItem {
    let display = if in_string || is_ident_like(name) {
        name.to_string()
    } else {
        let mut s = String::with_capacity(name.len() + 2);
        s.push('"');
        for c in name.chars() {
            match c {
                '\\' => s.push_str("\\\\"),
                '"' => s.push_str("\\\""),
                _ => s.push(c),
            }
        }
        s.push('"');
        s
    };
    CompletionItem {
        label: display.clone(),
        kind: Some(CompletionItemKind::ENUM_MEMBER),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range: replace_range,
            new_text: display,
        })),
        ..Default::default()
    }
}

/// Build a [`CompletionItem`] for a static method or module-level
/// decl reached through `Recv::|`. Same `text_edit`-based
/// replace-range plumbing as [`enum_variant_completion_item`] so
/// mid-ident invocation doesn't duplicate the typed prefix.
///
/// `detail` and `documentation` carry the signature + doc-comment of
/// the resolved decl so the popup's right-rail tooltip lights up the
/// same way it does for instance access (P15.2.4 / member completion).
fn static_completion_item(
    name: String,
    kind: CompletionItemKind,
    replace_range: lsp_types::Range,
    detail: Option<String>,
    documentation: Option<Documentation>,
) -> CompletionItem {
    CompletionItem {
        label: name.clone(),
        kind: Some(kind),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range: replace_range,
            new_text: name,
        })),
        detail,
        documentation,
        ..Default::default()
    }
}

/// Every reserved word the user can type at a statement / expression
/// position. Mirrors the keywords baked into the tree-sitter grammar
/// (`grammar.js`): the modifiers (`private`, `static`, `abstract`,
/// `native`), decl-level (`fn`, `type`, `enum`, `var`), control-flow
/// (`if`, `else`, `for`, `while`, `do`, `return`, `throw`, `try`,
/// `catch`, `at`, `in`), and expression-level (`is`, `as`, `null`,
/// `true`, `false`, `this`).
const ALL_KEYWORDS: &[&str] = &[
    "abstract", "as", "at", "catch", "do", "else", "enum", "false", "fn", "for", "if", "in", "is",
    "native", "null", "private", "return", "static", "this", "throw", "true", "try", "type", "var",
    "while",
];

// =============================================================================
// P15.2.3 — scope-aware ident completion
// =============================================================================

/// Emit a unified list of keywords + scope-visible names + project-wide
/// surface at an ident position. Mirrors the TS reference's
/// `Environment::suggest` (`packages/lang/src/analysis/environment.ts`)
/// — the per-suggestion `kind` is derived from each name's
/// `Definition` shape.
fn ident_or_keyword_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    if !is_keyword_position(text, node, cursor_byte) {
        return None;
    }
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<CompletionItem> = Vec::new();

    // Keywords first, alphabetic-sorted under `sort_text` so they land
    // toward the bottom of the suggestion popup (idents typically win).
    for kw in ALL_KEYWORDS {
        if !prefix_lower.is_empty() && !kw.starts_with(&prefix_lower) {
            continue;
        }
        if seen.insert((*kw).into()) {
            items.push(CompletionItem {
                label: (*kw).into(),
                kind: Some(CompletionItemKind::KEYWORD),
                insert_text: Some((*kw).into()),
                sort_text: Some(format!("z_{kw}")),
                ..Default::default()
            });
        }
    }

    // Scope-visible names — this module's HIR walked top-to-cursor.
    if let Some(module) = project.module(uri) {
        let names = scope_names_at(&module.hir, cursor_byte);
        for (name, kind, sort_pri, source) in names {
            if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let (detail, documentation) = scope_name_meta(module, project.arena(), &source);
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(kind),
                insert_text: Some(name),
                sort_text: Some(sort_pri.to_string()),
                detail,
                documentation,
                ..Default::default()
            });
        }
    }

    // Project surface — every cross-module top-level decl + primitives
    // + runtime types + native fn signatures. `module(uri)` guarded
    // to avoid double-emitting in-module decls.
    let in_module: std::collections::HashSet<String> = project
        .module(uri)
        .map(|m| {
            m.hir
                .module
                .as_ref()
                .map(|md| {
                    md.decls
                        .iter()
                        .filter_map(|d| m.hir.decls[*d].name())
                        .map(|n| m.hir.idents[n].text.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();

    for (name_sym, locs) in &project.index.decl_locations {
        let Some(name) = project.index.symbols.resolve(*name_sym) else {
            continue;
        };
        if in_module.contains(name) {
            continue;
        }
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        let kind = decl_locs_kind(project, locs);
        let (detail, documentation, description) = foreign_decl_completion_meta(project, locs);
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("y_{name}")),
            detail,
            documentation,
            label_details: description.map(|d| CompletionItemLabelDetails {
                description: Some(d),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    for name_sym in project.index.values.iter() {
        let Some(name) = project.index.symbols.resolve(*name_sym) else {
            continue;
        };
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("y_{name}")),
            ..Default::default()
        });
    }
    for name_sym in project.index.module_names.keys() {
        let Some(name) = project.index.symbols.resolve(*name_sym) else {
            continue;
        };
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(name.to_string()),
            sort_text: Some(format!("x_{name}")),
            ..Default::default()
        });
    }
    for name in greycat_analyzer_analysis::stdlib::BUILTIN_RUNTIME_TYPES {
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            continue;
        }
        if !seen.insert((*name).into()) {
            continue;
        }
        items.push(CompletionItem {
            label: (*name).into(),
            kind: Some(CompletionItemKind::CLASS),
            insert_text: Some((*name).into()),
            sort_text: Some(format!("y_{name}")),
            ..Default::default()
        });
    }

    if items.is_empty() {
        return None;
    }
    Some(items)
}

/// Post-process completion items so the LSP edit honors what the user
/// already typed:
///
/// 1. **Replace-range** — when the cursor sits mid-identifier
///    (`x.cha|rs()`), convert each item's `insert_text` into an
///    explicit `TextEdit` that spans the whole word
///    (`[ident_start..ident_end]`). Without this, editors that follow
///    the LSP literally insert at the cursor and leave the suffix
///    behind (`x.endsWith()chars()`). When there's no identifier under
///    the cursor (`x.|`), we leave `insert_text` alone — editors apply
///    their own prefix-deletion heuristic and the existing shape is
///    correct.
///
/// 2. **Call-parens** — for FUNCTION / METHOD items whose `(...)` isn't
///    already present immediately after the identifier, append `($0)`
///    and switch to `InsertTextFormat::SNIPPET` so the cursor lands
///    between the parens. The "parens already there" check probes the
///    byte right after `ident_end`, *not* the cursor — so on
///    `x.|chars()` (cursor before `chars`, parens after `chars`) the
///    snippet is suppressed because the user already opened the call.
///
/// Skips items already carrying a `SNIPPET` body (e.g. pragma
/// templates like `@library("$1", "$2")`) for the call-paren rewrite,
/// and skips items already carrying their own `text_edit` for the
/// replace-range conversion.
fn apply_call_paren_snippet(items: &mut [CompletionItem], text: &str, cursor_byte: usize) {
    let prefix_len = ident_prefix_at_cursor(text, cursor_byte).len();
    let suffix_len = ident_suffix_at_cursor(text, cursor_byte).len();
    let ident_start = cursor_byte.saturating_sub(prefix_len);
    let ident_end = cursor_byte + suffix_len;
    let parens_already_there = next_non_ws_is_open_paren(text.as_bytes(), ident_end);
    let replace_range =
        (suffix_len > 0).then(|| byte_range_to_lsp_range(text, &(ident_start..ident_end)));

    for item in items.iter_mut() {
        // 1) Append `($0)` to FUNCTION / METHOD items unless the
        //    surrounding source already opens the call. When the item
        //    already carries an explicit `text_edit` (e.g. enum-variant
        //    or static-completion shapes that bake in a replace-range),
        //    we mutate `text_edit.new_text` so the editor honors it;
        //    otherwise we mutate `insert_text`.
        if !parens_already_there
            && matches!(
                item.kind,
                Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::METHOD)
            )
            && !matches!(item.insert_text_format, Some(InsertTextFormat::SNIPPET))
        {
            if let Some(CompletionTextEdit::Edit(te)) = item.text_edit.as_mut() {
                te.new_text = format!("{}($0)", te.new_text);
            } else {
                let base = item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone());
                item.insert_text = Some(format!("{base}($0)"));
            }
            item.insert_text_format = Some(InsertTextFormat::SNIPPET);
        }

        // 2) When the cursor is mid-identifier, lift `insert_text` into
        //    a `TextEdit` covering the full word so the editor replaces
        //    `chars` (rather than inserting between `.` and `chars`).
        //    `text_edit` already set by an upstream emitter wins.
        if let Some(range) = replace_range
            && item.text_edit.is_none()
        {
            let new_text = item
                .insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone());
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
        }
    }
}

/// Word characters appearing immediately after `cursor_byte`. Mirrors
/// [`ident_prefix_at_cursor`]'s walk but goes forward instead of
/// backward.
fn ident_suffix_at_cursor(text: &str, cursor_byte: usize) -> &str {
    let bytes = text.as_bytes();
    let mut end = cursor_byte;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    &text[cursor_byte..end]
}

fn next_non_ws_is_open_paren(bytes: &[u8], cursor_byte: usize) -> bool {
    let mut i = cursor_byte;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'('
}

/// Render `(detail, documentation)` for a scope-visible name.
/// Module-level decls show their full signature plus their doc-comment;
/// locals / params surface their inferred type from `def_types` (no
/// docs, since locals carry none); generics return both as `None`.
fn scope_name_meta(
    module: &ModuleAnalysis,
    arena: &greycat_analyzer_types::TypeArena,
    source: &NameSource,
) -> (Option<String>, Option<Documentation>) {
    match source {
        NameSource::ModuleDecl(decl_id) => {
            let decl = &module.hir.decls[*decl_id];
            (
                Some(render_decl_signature(&module.hir, decl)),
                doc_to_markup(decl_doc(decl)),
            )
        }
        NameSource::Local(name_idx) | NameSource::Param(name_idx) => {
            let detail = module
                .analysis
                .def_types
                .get(name_idx)
                .map(|ty| greycat_analyzer_types::display(arena, *ty));
            (detail, None)
        }
        NameSource::Generic => (None, None),
    }
}

/// Render `(detail, documentation, description)` for a cross-module
/// decl surfaced via [`ProjectIndex::decl_locations`]. `detail` is
/// the foreign decl's signature; `description` is the home module's
/// stem (`model` for `file:///proj/src/model.gcl`); `documentation`
/// is the foreign decl's doc-comment. All three fall through to
/// `None` when the decl's home module isn't cached.
fn foreign_decl_completion_meta(
    project: &ProjectAnalysis,
    locs: &[(Uri, greycat_analyzer_hir::arena::Idx<Decl>)],
) -> (Option<String>, Option<Documentation>, Option<String>) {
    let Some((uri, decl_id)) = locs.first() else {
        return (None, None, None);
    };
    let Some(m) = project.module(uri) else {
        return (None, None, None);
    };
    let decl = &m.hir.decls[*decl_id];
    let detail = render_decl_signature(&m.hir, decl);
    let documentation = doc_to_markup(decl_doc(decl));
    let description = module_label_for_uri(uri);
    (Some(detail), documentation, Some(description))
}

/// Pick the `CompletionItemKind` for a name resolving through the
/// project index's decl table. When the name has multiple home
/// locations we pick the first; that's the same disambiguation policy
/// the resolver uses (P11.2).
fn decl_locs_kind(
    project: &ProjectAnalysis,
    locs: &[(Uri, greycat_analyzer_hir::arena::Idx<Decl>)],
) -> CompletionItemKind {
    if let Some((uri, decl_id)) = locs.first()
        && let Some(m) = project.module(uri)
    {
        match &m.hir.decls[*decl_id] {
            Decl::Fn(_) => CompletionItemKind::FUNCTION,
            Decl::Type(_) => CompletionItemKind::CLASS,
            Decl::Enum(_) => CompletionItemKind::ENUM,
            Decl::Var(_) => CompletionItemKind::VARIABLE,
            Decl::Pragma(_) => CompletionItemKind::CONSTANT,
        }
    } else {
        CompletionItemKind::TEXT
    }
}

/// Where a [`scope_names_at`] entry came from. Lets the completion
/// emitter reach back to the underlying decl / binding so it can
/// render a proper `detail` string for the popup (matches the TS
/// reference's `(<module>) name: T` quick-detail layout).
#[derive(Debug, Clone, Copy)]
enum NameSource {
    /// Top-level decl in the current module (`fn` / `type` / `enum` /
    /// `var`).
    ModuleDecl(greycat_analyzer_hir::arena::Idx<Decl>),
    /// Local `var x = …` binding. Carries the *binding* name idx so
    /// `def_types` resolves the inferred type.
    Local(greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>),
    /// Function parameter. Same payload as `Local` — capabilities
    /// disambiguate via `CompletionItemKind`.
    Param(greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>),
    /// Generic type parameter (`fn<T>` / `type Foo<T>`). No type to
    /// surface — kind alone tells the user enough.
    Generic,
}

/// Walk the HIR to collect every name visible at `cursor_byte`. Returns
/// `(name, completion_kind, sort_priority, source)` quadruples. Lower
/// sort_priority strings sort earlier — locals win over module decls.
///
/// This is a stand-alone walker that doesn't share state with the
/// resolver. The duplication is intentional — the resolver's `Cx`
/// builds full bindings (with `Definition` data), but completion only
/// needs the name + a kind hint, and re-running the resolver per
/// keystroke would be wasteful.
fn scope_names_at(
    hir: &greycat_analyzer_hir::Hir,
    cursor_byte: usize,
) -> Vec<(String, CompletionItemKind, &'static str, NameSource)> {
    use greycat_analyzer_hir::types::Decl as HD;
    let mut out: Vec<(String, CompletionItemKind, &'static str, NameSource)> = Vec::new();
    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    // Module-level decls are always visible (forward-ref allowed).
    for &decl_id in &module.decls {
        if let Some(name_id) = hir.decls[decl_id].name() {
            let name = hir.idents[name_id].text.clone();
            let kind = match &hir.decls[decl_id] {
                HD::Fn(_) => CompletionItemKind::FUNCTION,
                HD::Type(_) => CompletionItemKind::CLASS,
                HD::Enum(_) => CompletionItemKind::ENUM,
                HD::Var(_) => CompletionItemKind::VARIABLE,
                HD::Pragma(_) => continue,
            };
            out.push((name, kind, "n_", NameSource::ModuleDecl(decl_id)));
        }
    }
    // Descend into the declaration that contains the cursor.
    for &decl_id in &module.decls {
        let r = hir.decls[decl_id].byte_range();
        if !(r.start <= cursor_byte && cursor_byte <= r.end) {
            continue;
        }
        match &hir.decls[decl_id] {
            HD::Fn(d) => collect_fn_scope(hir, d, cursor_byte, &mut out),
            HD::Type(d) => {
                for g in &d.generics {
                    let n = hir.idents[*g].text.clone();
                    out.push((
                        n,
                        CompletionItemKind::TYPE_PARAMETER,
                        "g_",
                        NameSource::Generic,
                    ));
                }
                for &m_id in &d.methods {
                    let mr = hir.decls[m_id].byte_range();
                    if !(mr.start <= cursor_byte && cursor_byte <= mr.end) {
                        continue;
                    }
                    if let HD::Fn(fd) = &hir.decls[m_id] {
                        collect_fn_scope(hir, fd, cursor_byte, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn collect_fn_scope(
    hir: &greycat_analyzer_hir::Hir,
    fnd: &greycat_analyzer_hir::types::FnDecl,
    cursor_byte: usize,
    out: &mut Vec<(String, CompletionItemKind, &'static str, NameSource)>,
) {
    for g in &fnd.generics {
        let n = hir.idents[*g].text.clone();
        out.push((
            n,
            CompletionItemKind::TYPE_PARAMETER,
            "g_",
            NameSource::Generic,
        ));
    }
    for p in &fnd.params {
        let p = &hir.fn_params[*p];
        let n = hir.idents[p.name].text.clone();
        out.push((
            n,
            CompletionItemKind::VARIABLE,
            "a_",
            NameSource::Param(p.name),
        ));
    }
    if let Some(body) = fnd.body {
        collect_stmt_scope(hir, body, cursor_byte, out);
    }
}

fn cursor_in_block(block: &greycat_analyzer_hir::types::BlockStmt, cursor_byte: usize) -> bool {
    block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end
}

/// Walk a `BlockStmt` collecting cursor-visible names. Pre-cursor
/// `var` bindings surface; in-cursor stmts recurse. Replaces the
/// `HS::Block` arm of `collect_stmt_scope` since body-bearing
/// statements hold the block inline now and the byte-range bracket
/// comes from the block's own `byte_range` field (which is non-empty
/// even for `{ }` empty bodies — fixing the for-in scope-walker bug).
fn collect_block_scope(
    hir: &greycat_analyzer_hir::Hir,
    block: &greycat_analyzer_hir::types::BlockStmt,
    cursor_byte: usize,
    out: &mut Vec<(String, CompletionItemKind, &'static str, NameSource)>,
) {
    use greycat_analyzer_hir::types::Stmt as HS;
    if !(block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end) {
        return;
    }
    for s in &block.stmts {
        let r = stmt_byte_range(hir, *s);
        if r.end <= cursor_byte {
            if let HS::Var(lv) = &hir.stmts[*s] {
                let n = hir.idents[lv.name].text.clone();
                out.push((
                    n,
                    CompletionItemKind::VARIABLE,
                    "b_",
                    NameSource::Local(lv.name),
                ));
            }
        } else if r.start <= cursor_byte && cursor_byte <= r.end {
            collect_stmt_scope(hir, *s, cursor_byte, out);
        }
    }
}

fn collect_stmt_scope(
    hir: &greycat_analyzer_hir::Hir,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
    cursor_byte: usize,
    out: &mut Vec<(String, CompletionItemKind, &'static str, NameSource)>,
) {
    use greycat_analyzer_hir::types::Stmt as HS;
    match &hir.stmts[stmt_id] {
        HS::Block(b) => collect_block_scope(hir, b, cursor_byte, out),
        HS::If(s) => {
            collect_block_scope(hir, &s.then_branch, cursor_byte, out);
            if let Some(eb) = s.else_branch {
                let er = stmt_byte_range(hir, eb);
                if er.start <= cursor_byte && cursor_byte <= er.end {
                    collect_stmt_scope(hir, eb, cursor_byte, out);
                }
            }
        }
        HS::While(s) => collect_block_scope(hir, &s.body, cursor_byte, out),
        HS::DoWhile(s) => collect_block_scope(hir, &s.body, cursor_byte, out),
        HS::For(s) if cursor_in_block(&s.body, cursor_byte) => {
            if let Some(name_id) = s.init_name {
                let n = hir.idents[name_id].text.clone();
                out.push((
                    n,
                    CompletionItemKind::VARIABLE,
                    "b_",
                    NameSource::Local(name_id),
                ));
            }
            collect_block_scope(hir, &s.body, cursor_byte, out);
        }
        HS::ForIn(s) if cursor_in_block(&s.body, cursor_byte) => {
            for p in &s.params {
                let n = hir.idents[p.name].text.clone();
                out.push((
                    n,
                    CompletionItemKind::VARIABLE,
                    "b_",
                    NameSource::Local(p.name),
                ));
            }
            collect_block_scope(hir, &s.body, cursor_byte, out);
        }
        HS::Try(s) => {
            collect_block_scope(hir, &s.try_block, cursor_byte, out);
            if cursor_in_block(&s.catch_block, cursor_byte) {
                if let Some(err_id) = s.error_param {
                    let n = hir.idents[err_id].text.clone();
                    out.push((
                        n,
                        CompletionItemKind::VARIABLE,
                        "b_",
                        NameSource::Local(err_id),
                    ));
                }
                collect_block_scope(hir, &s.catch_block, cursor_byte, out);
            }
        }
        HS::At(s) => collect_block_scope(hir, &s.block, cursor_byte, out),
        _ => {}
    }
}

fn stmt_byte_range(
    hir: &greycat_analyzer_hir::Hir,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
) -> std::ops::Range<usize> {
    use greycat_analyzer_hir::types::Stmt as HS;
    match &hir.stmts[stmt_id] {
        HS::Block(b) => b.byte_range.clone(),
        HS::Var(s) => s.byte_range.clone(),
        HS::Assign(s) => s.byte_range.clone(),
        HS::If(s) => s.byte_range.clone(),
        HS::While(s) => s.byte_range.clone(),
        HS::DoWhile(s) => s.byte_range.clone(),
        HS::For(s) => s.byte_range.clone(),
        HS::ForIn(s) => s.byte_range.clone(),
        HS::Try(s) => s.byte_range.clone(),
        HS::At(s) => s.byte_range.clone(),
        HS::Expr(e) => hir.exprs[*e].byte_range(),
        HS::Return(Some(e)) => hir.exprs[*e].byte_range(),
        HS::Throw(e) => hir.exprs[*e].byte_range(),
        HS::Return(None) | HS::Break | HS::Continue => 0..0,
    }
}

// =============================================================================
// P15.2.4 — member completion after `.` / `->`
// =============================================================================

/// Member-access completion: when the cursor sits in `recv.|prop` /
/// `recv->|prop`, list the receiver type's attrs + methods. Cross-
/// module receivers consult `ProjectIndex::decl_locations` to navigate
/// to the foreign type's HIR.
///
/// Tolerant of error-recovery: when the user has typed `p.` (a
/// half-formed member access whose receiver lives inside an `ERROR`
/// node), the HIR doesn't carry an `Expr::Ident` for the receiver, so
/// we fall back to a CST-based ident lookup that consults the
/// resolver and `def_types` for the receiver's type.
fn member_completion(
    text: &str,
    root: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let prefix_start = cursor_byte.saturating_sub(typed.len());
    let bytes = text.as_bytes();
    if prefix_start > bytes.len() {
        return None;
    }
    // Determine separator: `.` (member) or `->` (arrow). `sep_start`
    // is the byte offset of the first separator char so we can build
    // a `.` → `->` rewrite range for P16.5's auto-deref nudge.
    let (recv_end, is_arrow, sep_start, sep_end) =
        if prefix_start >= 1 && bytes[prefix_start - 1] == b'.' {
            (prefix_start - 1, false, prefix_start - 1, prefix_start)
        } else if prefix_start >= 2
            && bytes[prefix_start - 2] == b'-'
            && bytes[prefix_start - 1] == b'>'
        {
            (prefix_start - 2, true, prefix_start - 2, prefix_start)
        } else {
            return None;
        };

    let module = project.module(uri)?;
    let arena = project.arena();
    let recv_ty = receiver_type_at(text, root, module, recv_end)?;
    let name = type_head_name(arena, recv_ty)?;

    // P16.5 — node-tag receivers auto-deref through their inner type:
    //   `n.|`  → list node's own members PLUS the inner type's members
    //            with a `.` → `->` rewrite edit.
    //   `n->|` → list the inner type's members directly.
    // The criterion mirrors the analyzer: single-arg node-tag generic.
    let inner_head: Option<String> = match (is_arrow, &arena.get(recv_ty).kind) {
        (_, greycat_analyzer_types::TypeKind::Generic { name: tag, args })
            if greycat_analyzer_types::is_node_tag(tag) && args.len() == 1 =>
        {
            type_head_name(arena, args[0]).map(|s| s.to_string())
        }
        _ => None,
    };

    let mut items: Vec<CompletionItem> = Vec::new();

    // For `->` on a node-tag receiver, skip the tag's own members
    // entirely — those are reachable via `.` only. The analyzer's
    // `arrow_deref_receiver` mirrors this dispatch.
    let list_tag_members = !(is_arrow && inner_head.is_some());
    if list_tag_members {
        if let Some(decl_id) = module.analysis.type_decls.get(name).copied()
            && let Decl::Type(td) = &module.hir.decls[decl_id]
        {
            collect_type_members(&module.hir, td, &prefix_lower, &mut items);
        }
        if items.is_empty()
            && let Some((foreign_uri, foreign_decl_id)) = project.index.locate_decl(name).first()
            && let Some(fmod) = project.module(foreign_uri)
            && let Decl::Type(td) = &fmod.hir.decls[*foreign_decl_id]
        {
            collect_type_members(&fmod.hir, td, &prefix_lower, &mut items);
        }
    }

    // Inner type's members. `.` rewrites to `->` via
    // `additional_text_edits`; `->` lands the items verbatim.
    if let Some(inner) = inner_head.as_deref() {
        let mut inner_items: Vec<CompletionItem> = Vec::new();
        if let Some(decl_id) = module.analysis.type_decls.get(inner).copied()
            && let Decl::Type(td) = &module.hir.decls[decl_id]
        {
            collect_type_members(&module.hir, td, &prefix_lower, &mut inner_items);
        }
        if inner_items.is_empty()
            && let Some((foreign_uri, foreign_decl_id)) = project.index.locate_decl(inner).first()
            && let Some(fmod) = project.module(foreign_uri)
            && let Decl::Type(td) = &fmod.hir.decls[*foreign_decl_id]
        {
            collect_type_members(&fmod.hir, td, &prefix_lower, &mut inner_items);
        }
        if !is_arrow && !inner_items.is_empty() {
            // `.` → `->` rewrite. The edit replaces the `.` byte with
            // `->` so the accepted item lands in the correct shape.
            let edit_range = lsp_types::Range {
                start: byte_to_position(text, sep_start),
                end: byte_to_position(text, sep_end),
            };
            for item in &mut inner_items {
                item.additional_text_edits = Some(vec![TextEdit {
                    range: edit_range,
                    new_text: "->".into(),
                }]);
            }
        }
        items.extend(inner_items);
    }

    // P19.17 — when the receiver is nullable and the user typed `.` /
    // `->` (not `?.` / `?->`), each accepted item should land as the
    // null-safe form: insert a `?` immediately before the separator
    // via `additional_text_edits`. The label is rewritten to `?.size`
    // so the user *sees* what they're inserting before they accept;
    // `filter_text` and `sort_text` keep the bare name so typing `s`
    // still filters to `size` and the list ordering is unchanged from
    // the non-null case.
    //
    // **Skip when the receiver chain has an upstream `?.`** — optional
    // chaining short-circuits the whole suffix, so `n?.resolve().|`
    // is runtime-safe even though `n?.resolve()`'s type is `String?`.
    // Only the leading `?.` is needed; pushing more would be noise.
    let receiver_nullable = arena.get(recv_ty).nullable;
    let already_nullsafe = recv_end > 0 && bytes[recv_end - 1] == b'?';
    let chain_protected = module
        .hir
        .exprs
        .iter()
        .filter(|(_, e)| e.byte_range().end == recv_end)
        .max_by_key(|(_, e)| e.byte_range().end - e.byte_range().start)
        .map(|(id, _)| {
            greycat_analyzer_analysis::lint::chain_has_upstream_nullsafe(&module.hir, id)
        })
        .unwrap_or(false);
    if receiver_nullable && !already_nullsafe && !chain_protected {
        let insert_at = lsp_types::Range {
            start: byte_to_position(text, sep_start),
            end: byte_to_position(text, sep_start),
        };
        let prefix = if is_arrow { "?->" } else { "?." };
        for item in &mut items {
            let bare = item.label.clone();
            let mut edits = item.additional_text_edits.take().unwrap_or_default();
            edits.push(TextEdit {
                range: insert_at,
                new_text: "?".into(),
            });
            item.additional_text_edits = Some(edits);
            item.label = format!("{prefix}{bare}");
            if item.filter_text.is_none() {
                item.filter_text = Some(bare.clone());
            }
            if item.sort_text.is_none() {
                item.sort_text = Some(bare);
            }
        }
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Find the receiver's `TypeId` whose CST span ends at `recv_end`.
/// Three-stage:
/// 1. **HIR fast path** — match an `Expr` whose byte_range ends there
///    against `analysis.expr_types`. Works for the common `recv.x`
///    case where the parser produced a `member_expr`.
/// 2. **CST + resolver fallback** — when the parser put the half-
///    formed access in an `ERROR` recovery node, no `Expr` covers
///    the receiver. We walk up the CST from the byte before
///    `recv_end`, find a named node whose `end_byte == recv_end`,
///    and try to resolve it via `Resolutions` + `def_types`.
/// 3. **CST + name-in-scope fallback** — when the receiver isn't
///    even in `Resolutions` (because the lowering skipped its
///    enclosing ERROR), look the receiver text up by name in the
///    enclosing fn's scope (params + nested var decls).
fn receiver_type_at(
    text: &str,
    root: tree_sitter::Node<'_>,
    module: &greycat_analyzer_analysis::project::ModuleAnalysis,
    recv_end: usize,
) -> Option<greycat_analyzer_types::TypeId> {
    if let Some((id, _)) = module
        .hir
        .exprs
        .iter()
        .filter(|(_, e)| e.byte_range().end == recv_end)
        .max_by_key(|(_, e)| e.byte_range().end - e.byte_range().start)
        && let Some(ty) = module.analysis.expr_types.get(&id).copied()
    {
        return Some(ty);
    }
    if recv_end == 0 {
        return None;
    }
    let leaf = node_at_offset(root, recv_end - 1)?;
    let mut cur = leaf;
    let recv_node = loop {
        if cur.is_named() && cur.end_byte() == recv_end {
            break cur;
        }
        match cur.parent() {
            Some(p) if p.end_byte() <= recv_end + 1 => cur = p,
            _ => return None,
        }
    };
    if recv_node.kind() != "ident" {
        return None;
    }
    let r = recv_node.byte_range();
    // Stage 2: ident already lowered into the HIR — resolver path.
    if let Some((ident_idx, _)) = module.hir.idents.iter().find(|(_, i)| i.byte_range == r) {
        use greycat_analyzer_analysis::resolver::Definition;
        if let Some(def) = module.resolutions.lookup(ident_idx) {
            let ident_for_lookup = match def {
                Definition::Param(id) | Definition::Local(id) | Definition::Generic(id) => Some(id),
                _ => None,
            };
            if let Some(id) = ident_for_lookup
                && let Some(ty) = module.analysis.def_types.get(&id).copied()
            {
                return Some(ty);
            }
        }
    }
    // Stage 3: ident dropped by lowering (lives inside an ERROR);
    // resolve by name lookup.
    let recv_text = text.get(r)?.to_string();
    lookup_name_type_at(&module.hir, &module.analysis, recv_end, &recv_text)
}

/// Walk the HIR for a Param / Local binding whose name matches `name`
/// and whose enclosing scope contains `cursor_byte`. Returns its
/// `TypeId` from `def_types`.
fn lookup_name_type_at(
    hir: &greycat_analyzer_hir::Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    cursor_byte: usize,
    name: &str,
) -> Option<greycat_analyzer_types::TypeId> {
    use greycat_analyzer_hir::types::Decl as HD;
    let module = hir.module.as_ref()?;
    for &decl_id in &module.decls {
        let r = hir.decls[decl_id].byte_range();
        if !(r.start <= cursor_byte && cursor_byte <= r.end) {
            continue;
        }
        match &hir.decls[decl_id] {
            HD::Fn(fnd) => {
                if let Some(t) = lookup_name_type_in_fn(hir, analysis, cursor_byte, fnd, name) {
                    return Some(t);
                }
            }
            HD::Type(td) => {
                for &m_id in &td.methods {
                    let mr = hir.decls[m_id].byte_range();
                    if !(mr.start <= cursor_byte && cursor_byte <= mr.end) {
                        continue;
                    }
                    if let HD::Fn(fnd) = &hir.decls[m_id]
                        && let Some(t) =
                            lookup_name_type_in_fn(hir, analysis, cursor_byte, fnd, name)
                    {
                        return Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn lookup_name_type_in_fn(
    hir: &greycat_analyzer_hir::Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    cursor_byte: usize,
    fnd: &greycat_analyzer_hir::types::FnDecl,
    name: &str,
) -> Option<greycat_analyzer_types::TypeId> {
    for p_id in &fnd.params {
        let p = &hir.fn_params[*p_id];
        if hir.idents[p.name].text == name {
            return analysis.def_types.get(&p.name).copied();
        }
    }
    if let Some(body) = fnd.body {
        return lookup_name_type_in_stmt(hir, analysis, cursor_byte, body, name);
    }
    None
}

fn lookup_name_type_in_block(
    hir: &greycat_analyzer_hir::Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    cursor_byte: usize,
    block: &greycat_analyzer_hir::types::BlockStmt,
    name: &str,
) -> Option<greycat_analyzer_types::TypeId> {
    use greycat_analyzer_hir::types::Stmt as HS;
    if !(block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end) {
        return None;
    }
    for s in &block.stmts {
        let r = stmt_byte_range(hir, *s);
        if r.end <= cursor_byte {
            if let HS::Var(lv) = &hir.stmts[*s]
                && hir.idents[lv.name].text == name
            {
                return analysis.def_types.get(&lv.name).copied();
            }
        } else if r.start <= cursor_byte
            && cursor_byte <= r.end
            && let Some(t) = lookup_name_type_in_stmt(hir, analysis, cursor_byte, *s, name)
        {
            return Some(t);
        }
    }
    None
}

fn lookup_name_type_in_stmt(
    hir: &greycat_analyzer_hir::Hir,
    analysis: &greycat_analyzer_analysis::analyzer::AnalysisResult,
    cursor_byte: usize,
    stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
    name: &str,
) -> Option<greycat_analyzer_types::TypeId> {
    use greycat_analyzer_hir::types::Stmt as HS;
    match &hir.stmts[stmt_id] {
        HS::Block(b) => lookup_name_type_in_block(hir, analysis, cursor_byte, b, name),
        HS::If(s) => {
            if let Some(t) =
                lookup_name_type_in_block(hir, analysis, cursor_byte, &s.then_branch, name)
            {
                return Some(t);
            }
            if let Some(eb) = s.else_branch {
                let er = stmt_byte_range(hir, eb);
                if er.start <= cursor_byte && cursor_byte <= er.end {
                    return lookup_name_type_in_stmt(hir, analysis, cursor_byte, eb, name);
                }
            }
            None
        }
        HS::While(s) => lookup_name_type_in_block(hir, analysis, cursor_byte, &s.body, name),
        HS::DoWhile(s) => lookup_name_type_in_block(hir, analysis, cursor_byte, &s.body, name),
        HS::For(s) => {
            if let Some(name_id) = s.init_name
                && hir.idents[name_id].text == name
            {
                return analysis.def_types.get(&name_id).copied();
            }
            lookup_name_type_in_block(hir, analysis, cursor_byte, &s.body, name)
        }
        HS::ForIn(s) => {
            for p in &s.params {
                if hir.idents[p.name].text == name {
                    return analysis.def_types.get(&p.name).copied();
                }
            }
            lookup_name_type_in_block(hir, analysis, cursor_byte, &s.body, name)
        }
        HS::Try(s) => {
            if let Some(t) =
                lookup_name_type_in_block(hir, analysis, cursor_byte, &s.try_block, name)
            {
                return Some(t);
            }
            if s.catch_block.byte_range.start <= cursor_byte
                && cursor_byte <= s.catch_block.byte_range.end
            {
                if let Some(err_id) = s.error_param
                    && hir.idents[err_id].text == name
                {
                    return analysis.def_types.get(&err_id).copied();
                }
                return lookup_name_type_in_block(hir, analysis, cursor_byte, &s.catch_block, name);
            }
            None
        }
        HS::At(s) => lookup_name_type_in_block(hir, analysis, cursor_byte, &s.block, name),
        _ => None,
    }
}

/// Read the head name of `id` from `arena` — the bare type name
/// stripped of nullability / generic args. Returns `None` for shapes
/// without a single name (lambdas, tuples, anonymous structures).
fn type_head_name(
    arena: &greycat_analyzer_types::TypeArena,
    id: greycat_analyzer_types::TypeId,
) -> Option<&str> {
    use greycat_analyzer_types::TypeKind;
    let t = arena.get(id);
    match &t.kind {
        TypeKind::Named { name } | TypeKind::Generic { name, .. } => Some(name),
        TypeKind::Primitive(p) => Some(p.name()),
        _ => None,
    }
}

/// Walk a `TypeDecl`'s attrs + methods and emit one `CompletionItem`
/// per name that survives the `prefix_lower` filter. Skips abstract /
/// native methods only on the static-completion side (P15.2.5);
/// instance access lists everything.
fn collect_type_members(
    hir: &greycat_analyzer_hir::Hir,
    td: &greycat_analyzer_hir::types::TypeDecl,
    prefix_lower: &str,
    items: &mut Vec<CompletionItem>,
) {
    for attr_id in &td.attrs {
        let a = &hir.type_attrs[*attr_id];
        let name = hir.idents[a.name].text.clone();
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        let ty =
            a.ty.map(|t| render_type_ref(hir, t))
                .unwrap_or_else(|| "any".into());
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            insert_text: Some(name.clone()),
            detail: Some(format!("{name}: {ty}")),
            documentation: doc_to_markup(a.doc.as_deref()),
            ..Default::default()
        });
    }
    for method_id in &td.methods {
        let Decl::Fn(m) = &hir.decls[*method_id] else {
            continue;
        };
        // `static` methods don't apply to instance access (P15.2.5
        // owns the static-call path); skip them here.
        if m.modifiers.static_ {
            continue;
        }
        let name = hir.idents[m.name].text.clone();
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::METHOD),
            insert_text: Some(name),
            detail: Some(render_fn_signature(hir, m)),
            documentation: doc_to_markup(m.doc.as_deref()),
            ..Default::default()
        });
    }
}

/// Wrap a doc-comment paragraph as LSP markup so completion-item
/// tooltips render it correctly. Returns `None` for missing / blank
/// docs so the field stays absent on the wire.
fn doc_to_markup(doc: Option<&str>) -> Option<Documentation> {
    let trimmed = doc?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: trimmed.to_string(),
    }))
}

// =============================================================================
// P15.2.5 — static completion after `::`
// =============================================================================

/// Static-access completion: when the cursor sits in `Type::|prop` or
/// `module::|name`, list the type's static methods or the module's
/// top-level decls. Receiver detection:
/// - Walk back from the cursor over `[A-Za-z0-9_]*` (typed prefix).
/// - Confirm `::` precedes the prefix.
/// - Walk back further over `[A-Za-z0-9_]+` to capture the receiver
///   ident.
///
/// Two dispatch shapes:
/// 1. `Type::|` — receiver matches a known type decl. Emit its
///    `static` methods. Chain context (`module::Type::|`) is
///    transparent: we still look the type up by name.
/// 2. `module::|` — receiver matches `ProjectIndex::module_names`.
///    Emit that module's top-level decls.
fn static_completion(
    text: &str,
    cursor_byte: usize,
    project: &greycat_analyzer_analysis::project::ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    let ctx = static_receiver_at(text, cursor_byte)?;
    let prefix_lower = ctx.typed.to_lowercase();
    let replace_range = lsp_types::Range {
        start: byte_to_position(text, ctx.replace_range.start),
        end: byte_to_position(text, ctx.replace_range.end),
    };

    let mut items: Vec<CompletionItem> = Vec::new();

    // Receiver branch: type-decl → static methods, enum-decl →
    // variants. The `recv` text matches a top-level decl name in some
    // module (resolved through the project decl table).
    if let Some((foreign_uri, foreign_decl_id)) = project.index.locate_decl(&ctx.recv).first()
        && let Some(fmod) = project.module(foreign_uri)
    {
        match &fmod.hir.decls[*foreign_decl_id] {
            Decl::Type(td) => {
                for method_id in &td.methods {
                    let Decl::Fn(m) = &fmod.hir.decls[*method_id] else {
                        continue;
                    };
                    if !m.modifiers.static_ {
                        continue;
                    }
                    let name = fmod.hir.idents[m.name].text.clone();
                    if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                        continue;
                    }
                    let detail = Some(render_fn_signature(&fmod.hir, m));
                    let documentation = doc_to_markup(m.doc.as_deref());
                    items.push(static_completion_item(
                        name,
                        CompletionItemKind::METHOD,
                        replace_range,
                        detail,
                        documentation,
                    ));
                }
            }
            Decl::Enum(ed) => {
                // `Foo::|` where `Foo` is an enum — surface every
                // variant. Common in stdlib: `core::TimeZone` ships
                // 600+ IANA-spelled variants (`"Africa/Abidjan"`,
                // `"America/New_York"`, …) so we keep the per-item
                // path allocation-light.
                for f in &ed.fields {
                    let name = fmod.hir.idents[fmod.hir.enum_fields[*f].name].text.as_str();
                    if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                        continue;
                    }
                    items.push(enum_variant_completion_item(
                        name,
                        ctx.in_string,
                        replace_range,
                    ));
                }
            }
            _ => {}
        }
    }

    // Module-receiver branch: enumerate the module's top-level decls.
    if let Some(mod_uri) = project.index.module_uri(&ctx.recv).cloned()
        && let Some(mod_analysis) = project.module(&mod_uri)
        && let Some(module_hir) = mod_analysis.hir.module.as_ref()
    {
        for &decl_id in &module_hir.decls {
            let Some(name_id) = mod_analysis.hir.decls[decl_id].name() else {
                continue;
            };
            let name = mod_analysis.hir.idents[name_id].text.clone();
            if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
                continue;
            }
            let decl = &mod_analysis.hir.decls[decl_id];
            let kind = match decl {
                Decl::Fn(_) => CompletionItemKind::FUNCTION,
                Decl::Type(_) => CompletionItemKind::CLASS,
                Decl::Enum(_) => CompletionItemKind::ENUM,
                Decl::Var(_) => CompletionItemKind::VARIABLE,
                Decl::Pragma(_) => continue,
            };
            let detail = Some(render_decl_signature(&mod_analysis.hir, decl));
            let documentation = doc_to_markup(decl_doc(decl));
            items.push(static_completion_item(
                name,
                kind,
                replace_range,
                detail,
                documentation,
            ));
        }
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

/// Receiver context for `Recv::|prop` / `Recv::"prop|"` completion.
///
/// `replace_range` covers the whole property token at the cursor —
/// from the start of the typed prefix to the end of the surrounding
/// ident run (or to the closing `"` for string-mode). Threading this
/// into every completion item's `text_edit` is what keeps "ask for
/// completion in the middle of a word" honest: the accepted text
/// replaces the existing word instead of doubling it via a naive
/// `insert_text` insertion at the cursor.
struct StaticRecvCtx {
    /// Receiver name (`Foo`, `runtime`, …). Plain ident text, no
    /// quotes / separators.
    recv: String,
    /// What the user has typed so far at the cursor; the prefix
    /// filter for completion items. Always derived from the source
    /// chars from `replace_range.start..cursor_byte`.
    typed: String,
    /// Replace-range as UTF-8 byte offsets. For ident-mode this is
    /// `[prop_start..prop_end]` covering every alphanumeric run
    /// around the cursor. For string-mode this is the inner content
    /// span `[after_open_quote..before_close_quote]` (cursor INSIDE
    /// the quotes, opening/closing kept).
    replace_range: std::ops::Range<usize>,
    /// `true` when the cursor sits inside `Recv::"…|"`. The opening
    /// `"` is in the buffer, so completion items emit bare names
    /// (no re-quoting).
    in_string: bool,
}

/// Walk back from `cursor_byte` to extract the static-access receiver
/// and the byte range to replace. Returns `None` when the cursor
/// isn't in a `Recv::|` / `Recv::"|"` shape.
fn static_receiver_at(text: &str, cursor_byte: usize) -> Option<StaticRecvCtx> {
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());

    // String-mode: the cursor sits inside `recv::"…|…"`. Walk back
    // over non-`"` chars to find the opening quote, then forward
    // from the cursor over non-`"` chars to find the closing quote
    // (or EOL — we stop at `\n` so an unterminated string doesn't
    // swallow the rest of the file).
    {
        let mut i = cap;
        while i > 0 && bytes[i - 1] != b'"' && bytes[i - 1] != b'\n' {
            i -= 1;
        }
        if i >= 3 && bytes[i - 1] == b'"' && bytes[i - 2] == b':' && bytes[i - 3] == b':' {
            let inner_start = i;
            let mut j = cap;
            while j < bytes.len() && bytes[j] != b'"' && bytes[j] != b'\n' {
                j += 1;
            }
            let inner_end = j;
            let typed = text.get(inner_start..cap).unwrap_or("").to_string();
            let sep_start = i - 3;
            let recv = walk_back_receiver(bytes, sep_start, text)?;
            return Some(StaticRecvCtx {
                recv,
                typed,
                replace_range: inner_start..inner_end,
                in_string: true,
            });
        }
    }

    // Ident-mode: `recv::|prop`. Walk back over `[A-Za-z0-9_]` for
    // the prefix and forward from the cursor over the same class
    // for the rest of the surrounding ident — so completion in the
    // middle of `Foo::Tim|eZone` replaces the whole `TimeZone` run.
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_start = cap.saturating_sub(typed.len());
    if prefix_start < 2 || bytes[prefix_start - 1] != b':' || bytes[prefix_start - 2] != b':' {
        return None;
    }
    let mut j = cap;
    while j < bytes.len() {
        let b = bytes[j];
        if b.is_ascii_alphanumeric() || b == b'_' {
            j += 1;
        } else {
            break;
        }
    }
    let sep_start = prefix_start - 2;
    let recv = walk_back_receiver(bytes, sep_start, text)?;
    Some(StaticRecvCtx {
        recv,
        typed,
        replace_range: prefix_start..j,
        in_string: false,
    })
}

/// Shared receiver walk-back used by both static-completion modes
/// (ident property and string property). Walks left from `sep_start`
/// over `[A-Za-z0-9_]` chars and slices the receiver text. Returns
/// `None` when no receiver run is present.
fn walk_back_receiver(bytes: &[u8], sep_start: usize, text: &str) -> Option<String> {
    let mut i = sep_start;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i == sep_start {
        return None;
    }
    text.get(i..sep_start).map(str::to_string)
}

// =============================================================================
// P15.2.6 — type-position completion
// =============================================================================

/// Type-position completion: when the cursor sits inside a
/// `type_ident` slot — `var x: |`, `<|`, `extends |`, fn param /
/// return type, etc. — emit only type-shaped names (in-module type /
/// enum decls + every type registered in the project's
/// [`ProjectIndex`] + runtime types + primitives) prefix-filtered.
fn type_position_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &greycat_analyzer_analysis::project::ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    ancestor_with_kind(node, "type_ident")?;
    // Bail if we're on the RHS of a member / static / annotation chain.
    let bytes = text.as_bytes();
    let cap = cursor_byte.min(bytes.len());
    let mut i = cap;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    if i > 0 && matches!(bytes[i - 1], b'.' | b'>' | b'@') {
        return None;
    }
    // Allow `module::|Type` — the static branch already handles that.
    if i >= 2 && bytes[i - 1] == b':' && bytes[i - 2] == b':' {
        return None;
    }
    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut items: Vec<CompletionItem> = Vec::new();
    let push = |items: &mut Vec<CompletionItem>,
                seen: &mut std::collections::HashSet<String>,
                name: &str,
                kind: CompletionItemKind| {
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(&prefix_lower) {
            return;
        }
        if seen.insert(name.into()) {
            items.push(CompletionItem {
                label: name.into(),
                kind: Some(kind),
                insert_text: Some(name.into()),
                ..Default::default()
            });
        }
    };

    // In-module decls (always visible at top level).
    if let Some(module) = project.module(uri)
        && let Some(m) = module.hir.module.as_ref()
    {
        for decl_id in &m.decls {
            let kind = match &module.hir.decls[*decl_id] {
                Decl::Type(_) => CompletionItemKind::CLASS,
                Decl::Enum(_) => CompletionItemKind::ENUM,
                _ => continue,
            };
            if let Some(name_id) = module.hir.decls[*decl_id].name() {
                let name = module.hir.idents[name_id].text.clone();
                push(&mut items, &mut seen, &name, kind);
            }
        }
    }
    // In-scope generic type-params from the enclosing fn / type.
    if let Some(module) = project.module(uri) {
        for (name, kind, _, _) in scope_names_at(&module.hir, cursor_byte) {
            if matches!(kind, CompletionItemKind::TYPE_PARAMETER) {
                push(&mut items, &mut seen, &name, kind);
            }
        }
    }
    // Project-level type / enum decls.
    for (name_sym, locs) in &project.index.decl_locations {
        let Some(name) = project.index.symbols.resolve(*name_sym) else {
            continue;
        };
        if let Some((u, d)) = locs.first()
            && let Some(m) = project.module(u)
        {
            let kind = match &m.hir.decls[*d] {
                Decl::Type(_) => CompletionItemKind::CLASS,
                Decl::Enum(_) => CompletionItemKind::ENUM,
                _ => continue,
            };
            push(&mut items, &mut seen, name, kind);
        }
    }
    // Runtime types.
    for &name in greycat_analyzer_analysis::stdlib::BUILTIN_RUNTIME_TYPES {
        push(&mut items, &mut seen, name, CompletionItemKind::CLASS);
    }
    // Primitives.
    for &p in &[
        "int", "float", "bool", "char", "String", "time", "duration", "geo", "any",
    ] {
        push(&mut items, &mut seen, p, CompletionItemKind::CLASS);
    }
    // Module names — type slots can read `module::Foo`, so module
    // names are valid here as the leading segment.
    for name_sym in project.index.module_names.keys() {
        let Some(name) = project.index.symbols.resolve(*name_sym) else {
            continue;
        };
        push(&mut items, &mut seen, name, CompletionItemKind::MODULE);
    }

    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

// =============================================================================
// P15.2.7 — object literal field completion
// =============================================================================

/// Object-literal field completion: cursor sits inside an
/// `object_initializers` / `object_fields` body (`Type { | }` or
/// `Type { x: 1, | }`). Resolves the surrounding `object_expr`'s
/// `type_ident` head, then emits the type's `attrs` as `FIELD`
/// completions, skipping ones already named in the literal.
fn object_field_completion(
    text: &str,
    node: tree_sitter::Node<'_>,
    cursor_byte: usize,
    uri: &Uri,
    project: &greycat_analyzer_analysis::project::ProjectAnalysis,
) -> Option<Vec<CompletionItem>> {
    // Walk up to find the enclosing `object_initializers` /
    // `object_fields` block. `object_field` walks one extra level.
    let body = ancestor_with_kind(node, "object_initializers")
        .or_else(|| ancestor_with_kind(node, "object_fields"))?;
    let object_expr = ancestor_with_kind(body, "object_expr")?;
    let type_ident = children_by_field_name(object_expr, "type")?;
    let type_name_node = type_ident.named_child(0)?;
    if type_name_node.kind() != "ident" {
        return None;
    }
    let type_name = text.get(type_name_node.byte_range())?.to_string();

    let typed = ident_prefix_at_cursor(text, cursor_byte);
    let prefix_lower = typed.to_lowercase();

    // Find the type's HIR (in-module first, then cross-module).
    let module = project.module(uri)?;
    let mut items: Vec<CompletionItem> = Vec::new();
    if let Some(decl_id) = module.analysis.type_decls.get(&type_name).copied()
        && let Decl::Type(td) = &module.hir.decls[decl_id]
    {
        emit_attrs(&module.hir, td, &prefix_lower, &mut items);
    }
    if items.is_empty()
        && let Some((foreign_uri, foreign_decl_id)) = project.index.locate_decl(&type_name).first()
        && let Some(fmod) = project.module(foreign_uri)
        && let Decl::Type(td) = &fmod.hir.decls[*foreign_decl_id]
    {
        emit_attrs(&fmod.hir, td, &prefix_lower, &mut items);
    }
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

fn emit_attrs(
    hir: &greycat_analyzer_hir::Hir,
    td: &greycat_analyzer_hir::types::TypeDecl,
    prefix_lower: &str,
    items: &mut Vec<CompletionItem>,
) {
    for attr_id in &td.attrs {
        let a = &hir.type_attrs[*attr_id];
        let name = hir.idents[a.name].text.clone();
        if !prefix_lower.is_empty() && !name.to_lowercase().starts_with(prefix_lower) {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            insert_text: Some(format!("{name}: ")),
            ..Default::default()
        });
    }
}

/// Helper mirroring `tree_sitter::Node::child_by_field_name` that
/// returns an `Option`.
fn children_by_field_name<'a>(
    node: tree_sitter::Node<'a>,
    field: &str,
) -> Option<tree_sitter::Node<'a>> {
    node.child_by_field_name(field)
}

// =============================================================================
// On-demand diagnostics for capabilities that don't sit on the publish path
// =============================================================================

/// Single-file pipeline (HIR lower → resolver → analyzer + lints) against
/// `text`, returning every finding as `lsp_types::Diagnostic`. Used by
/// the legacy [`code_actions`] shim — the LSP server's
/// `code_actions_handler` reads from the project cache via
/// [`code_actions_with_project`] / [`diagnostics_from_module`] instead.
pub(crate) fn current_diagnostics(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
) -> Vec<Diagnostic> {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let (_arena, analysis) = greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions);
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

/// Project-aware diagnostics — read the cached analyzer + lints from
/// the [`ModuleAnalysis`] entry for this module and convert each
/// finding to an `lsp_types::Diagnostic`. Mirrors the body of the cli
/// `lint` command's per-module conversion (P14.5) so the LSP and the
/// CLI surface the same diagnostic shape.
///
/// `lint_libs` opts into emitting lint diagnostics for non-project
/// modules (anything under `lib/<name>/`). Default for the LSP is
/// `false` — most users don't want warnings about vendored libraries
/// they don't own. The VS Code extension exposes this via the
/// `greycat-analyzer.lintLibs` setting; the CLI uses `--lint-libs`.
/// Type-relation / semantic diagnostics are unaffected — those always
/// surface regardless of the module's home library, so cross-module
/// shape mismatches can't hide behind a library boundary.
pub fn diagnostics_from_module(
    text: &str,
    module: &ModuleAnalysis,
    lint_libs: bool,
) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = module
        .analysis
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
    if lint_libs || module.lib == "project" {
        for lint in &module.lints {
            out.push(Diagnostic {
                range: byte_range_to_lsp_range(text, &lint.byte_range),
                severity: Some(match lint.severity {
                    LintSeverity::Error => DiagnosticSeverity::ERROR,
                    LintSeverity::Warning => DiagnosticSeverity::WARNING,
                    LintSeverity::Hint => DiagnosticSeverity::HINT,
                }),
                code: Some(NumberOrString::String(lint.rule.into())),
                source: Some("lint".into()),
                message: lint.message.clone(),
                ..Default::default()
            });
        }
    }
    out
}

fn byte_range_to_lsp_range(text: &str, range: &std::ops::Range<usize>) -> lsp_types::Range {
    lsp_types::Range {
        start: byte_to_position(text, range.start),
        end: byte_to_position(text, range.end),
    }
}
