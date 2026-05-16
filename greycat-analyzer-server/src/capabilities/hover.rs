//! Hover handlers — single-file + project-aware variants, plus the
//! tower of helpers that render decls / members / type-refs into
//! markdown. `render_type_ref` is `pub(super)` so signature_help can
//! reuse the same renderer.

use std::ops::Range;

use greycat_analyzer_analysis::analyzer::AnalysisResult;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::{Definition, Resolutions, resolve};
use greycat_analyzer_core::{SourceManager, Symbol, SymbolTable, TypeArena, TypeId, TypeKind};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, FnDecl, Ident, TypeDecl};
use greycat_analyzer_syntax::cst::{ancestors, node_at_offset};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position, Uri};
use rustc_hash::FxHashMap;

use crate::conv::{byte_range_to_lsp, position_to_byte};

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

// P15.1
/// Hover with project context. Restores cross-module hover
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
                project.symbols(),
                &module.resolutions,
                &module.analysis,
                project.arena(),
                project.decl_registry(),
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
                        let markdown = render_decl_hover_markdown(
                            &module.hir,
                            project.symbols(),
                            decl,
                            None,
                            None,
                        );
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
                .filter(|(_, e)| !matches!(e, Expr::Ident { .. }))
                .find(|(_, e)| {
                    let er = e.byte_range();
                    !er.is_empty() && er == r
                })
                && let Some(ty) = module.analysis.expr_types.get(&expr_id)
            {
                let label = format!(
                    "{}: {}",
                    short_expr_label(&module.hir, project.symbols(), expr),
                    project.display_type(*ty),
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
    #[allow(dead_code)] // reserved for future cross-module hover content
    manager: &'a SourceManager,
}

fn hover_inner(text: &str, lib: &str, root: tree_sitter::Node<'_>, pos: Position) -> Option<Hover> {
    let byte = position_to_byte(text, pos);
    let node = node_at_offset(root, byte)?;
    if !node.is_named() {
        return None;
    }

    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);
    let (arena, decl_registry, analysis) =
        greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions, &symbols);

    // --- Layer 1: ident-based hover (params / locals / decls / builtins).
    if node.kind() == "ident"
        && let Some((ident_idx, ident)) = hir
            .idents
            .iter()
            .find(|(_, i)| i.byte_range == node.byte_range())
    {
        if let Some(markdown) = ident_hover_markdown(
            &hir,
            &symbols,
            &resolutions,
            &analysis,
            &arena,
            &decl_registry,
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
                    let markdown = render_decl_hover_markdown(&hir, &symbols, decl, None, None);
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
            .filter(|(_, e)| !matches!(e, Expr::Ident { .. }))
            .find(|(_, e)| {
                let er = e.byte_range();
                !er.is_empty() && er == r
            })
            && let Some(ty) = analysis.expr_types.get(&expr_id)
        {
            let label = format!(
                "{}: {}",
                short_expr_label(&hir, &symbols, expr),
                greycat_analyzer_analysis::project::display_type(
                    &arena,
                    &decl_registry,
                    &symbols,
                    *ty,
                ),
            );
            return Some(hover_from_markdown(wrap_code(&label), r, text));
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn ident_hover_markdown(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    decl_registry: &greycat_analyzer_analysis::well_known::DeclRegistry,
    ident_idx: Idx<Ident>,
    ident: &Ident,
    project: Option<HoverProjectCtx<'_>>,
) -> Option<String> {
    let ident_name = &symbols[ident.symbol];
    // P6.3: property idents in `a.b` aren't in `Resolutions` — they
    // bind to a `TypeAttr` / method via the analyzer's member pass.
    // Check that first so member hovers render with the right shape.
    //
    // When a project context is available, build a substitution map
    // from the receiver's instantiation so the rendered signature
    // shows the concrete generic args (`fn add(value: String): null`)
    // instead of the declared param names (`fn add(value: T): null`).
    if let Some(member) = analysis.member_lookup(ident_idx) {
        let subst_owner = project.and_then(|ctx| {
            let recv_ty = receiver_ty_for_property(hir, analysis, ident_idx)?;
            ctx.project.method_subst_from_receiver_ty(recv_ty)
        });
        let render_ctx = project
            .zip(subst_owner.as_ref())
            .map(|(ctx, subst)| RenderCtx {
                project: ctx.project,
                subst,
            });
        return Some(member_hover_markdown(
            hir,
            symbols,
            member,
            ident,
            render_ctx.as_ref(),
        ));
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
        let subst_owner = receiver_ty_for_property(hir, analysis, ident_idx)
            .and_then(|recv_ty| ctx.project.method_subst_from_receiver_ty(recv_ty));
        let render_ctx = subst_owner.as_ref().map(|subst| RenderCtx {
            project: ctx.project,
            subst,
        });
        return Some(foreign_member_hover_markdown(
            &fmod.hir,
            symbols,
            &foreign.member,
            ident,
            &provenance,
            render_ctx.as_ref(),
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
            symbols,
            &fmod.hir.decls[fdecl.decl],
            Some(&provenance),
            None,
        ));
    }
    match resolutions.lookup(ident_idx)? {
        Definition::Param(name) | Definition::Local(name) => {
            analysis.def_types.get(&name).map(|ty| {
                wrap_code(&format!(
                    "{}: {}",
                    ident_name,
                    greycat_analyzer_analysis::project::display_type(
                        arena,
                        decl_registry,
                        symbols,
                        *ty,
                    ),
                ))
            })
        }
        Definition::Decl(decl_id) => Some(render_decl_hover_markdown(
            hir,
            symbols,
            &hir.decls[decl_id],
            None,
            None,
        )),
        Definition::Generic(_) => Some(wrap_code(&format!("(type parameter) {}", ident_name))),
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
                    symbols,
                    &fmod.hir.decls[decl],
                    Some(&provenance),
                    None,
                ));
            }
            Some(wrap_code(&format!("(project) {}", ident_name)))
        }
        Definition::Project => Some(wrap_code(&format!("(runtime built-in) {}", ident_name))),
    }
}

/// Hover markdown for a property ident bound by  member resolution
/// (`a.b` / `a->b`). Renders attribute / method shape with the
/// declared / inferred return type when available.
fn member_hover_markdown(
    hir: &Hir,
    symbols: &SymbolTable,
    member: greycat_analyzer_analysis::analyzer::MemberDef,
    ident: &greycat_analyzer_hir::types::Ident,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    match member {
        MemberDef::Attr(attr_id) => {
            let attr = &hir.type_attrs[attr_id];
            let ty_str = attr
                .ty
                .map(|t| render_type_ref_with_subst(hir, symbols, t, ctx))
                .unwrap_or_else(|| "any".into());
            let mut out = String::new();
            push_doc_section(&mut out, attr.doc.as_deref());
            out.push_str(&wrap_code(&format!(
                "{}: {}",
                &symbols[ident.symbol], ty_str
            )));
            out
        }
        MemberDef::Method(decl_id) => {
            let decl = &hir.decls[decl_id];
            render_decl_hover_markdown(hir, symbols, decl, None, ctx)
        }
    }
}

// P15.x
/// Cross-module variant of [`member_hover_markdown`]. Reads
/// the foreign HIR for the attr / method and appends an italic
/// `*defined in `<module>`*` footnote.
fn foreign_member_hover_markdown(
    foreign_hir: &Hir,
    symbols: &SymbolTable,
    member: &greycat_analyzer_analysis::analyzer::MemberDef,
    ident: &greycat_analyzer_hir::types::Ident,
    provenance: &str,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    use greycat_analyzer_analysis::analyzer::MemberDef;
    let mut out = match member {
        MemberDef::Attr(attr_id) => {
            let attr = &foreign_hir.type_attrs[*attr_id];
            let ty_str = attr
                .ty
                .map(|t| render_type_ref_with_subst(foreign_hir, symbols, t, ctx))
                .unwrap_or_else(|| "any".into());
            let mut s = String::new();
            push_doc_section(&mut s, attr.doc.as_deref());
            s.push_str(&wrap_code(&format!(
                "{}: {}",
                &symbols[ident.symbol], ty_str
            )));
            s
        }
        MemberDef::Method(decl_id) => {
            let decl = &foreign_hir.decls[*decl_id];
            render_decl_hover_markdown(foreign_hir, symbols, decl, None, ctx)
        }
    };
    out.push_str("\n\n*defined in `");
    out.push_str(provenance);
    out.push_str("`*");
    out
}

// P15.1
/// Render a top-level decl as hover markdown. Output layout:
/// optional doc paragraph, then a ```greycat fenced code block with the
/// signature, then (when `provenance` is `Some`) an italic
/// "*defined in `<name>`*" footnote. `provenance` is supplied only for
/// cross-module idents — intra-module uses pass `None`.
fn render_decl_hover_markdown(
    hir: &Hir,
    symbols: &SymbolTable,
    decl: &Decl,
    provenance: Option<&str>,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    let mut out = String::new();
    let signature = render_decl_signature(hir, symbols, decl, ctx);
    out.push_str(&wrap_code(&signature));
    out.push('\n');
    push_doc_section(&mut out, decl_doc(decl));
    if let Some(prov) = provenance {
        out.push_str("\n*defined in `");
        out.push_str(prov);
        out.push_str("`*");
    }
    out
}

pub(super) fn decl_doc(decl: &Decl) -> Option<&str> {
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
///
/// `ctx` carries an optional receiver-instantiation substitution: when
/// present, generic params on the owning type are rendered as the
/// receiver's concrete args (e.g. `arr: Array<String>` hovering on
/// `arr.add` renders `fn add(value: String): null` instead of the
/// declared `fn add(value: T): null`). The free-function / type-decl
/// paths pass `None` and the renderer behaves byte-identically to the
/// unsubst form.
pub(super) fn render_decl_signature(
    hir: &Hir,
    symbols: &SymbolTable,
    decl: &Decl,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    match decl {
        Decl::Fn(d) => render_fn_signature(hir, symbols, d, ctx),
        Decl::Type(d) => render_type_signature(hir, symbols, d),
        Decl::Enum(d) => format!("enum {}", &symbols[hir.idents[d.name].symbol]),
        Decl::Var(d) => {
            let ty =
                d.ty.map(|t| render_type_ref_with_subst(hir, symbols, t, ctx))
                    .unwrap_or_else(|| "any".into());
            format!("var {}: {}", &symbols[hir.idents[d.name].symbol], ty)
        }
        Decl::Pragma(p) => format!("@{}", &symbols[hir.idents[p.name].symbol]),
    }
}

pub(super) fn render_fn_signature(
    hir: &Hir,
    symbols: &SymbolTable,
    fnd: &FnDecl,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    let name = &symbols[hir.idents[fnd.name].symbol];
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
            out.push_str(&symbols[hir.idents[*g].symbol]);
        }
        out.push('>');
    }
    out.push('(');
    for (i, param_id) in fnd.params.iter().enumerate() {
        let p = &hir.fn_params[*param_id];
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&symbols[hir.idents[p.name].symbol]);
        out.push_str(": ");
        match p.ty {
            Some(t) => out.push_str(&render_type_ref_with_subst(hir, symbols, t, ctx)),
            None => out.push_str("any"),
        }
    }
    out.push(')');
    if let Some(ret) = fnd.return_type {
        out.push_str(": ");
        out.push_str(&render_type_ref_with_subst(hir, symbols, ret, ctx));
    }
    out
}

fn render_type_signature(hir: &Hir, symbols: &SymbolTable, td: &TypeDecl) -> String {
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
    out.push_str(&symbols[hir.idents[td.name].symbol]);
    if !td.generics.is_empty() {
        out.push('<');
        for (i, g) in td.generics.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&symbols[hir.idents[*g].symbol]);
        }
        out.push('>');
    }
    if let Some(parent) = td.supertype {
        out.push_str(" extends ");
        out.push_str(&render_type_ref(hir, symbols, parent));
    }
    out
}

pub(super) fn render_type_ref(
    hir: &Hir,
    symbols: &SymbolTable,
    type_ref: Idx<greycat_analyzer_hir::types::TypeRef>,
) -> String {
    render_type_ref_with_subst(hir, symbols, type_ref, None)
}

/// Receiver-instantiation context used by the substitution-aware
/// renderers. Built by `make_render_ctx` from a receiver `TypeId` —
/// hover / completion thread it through `render_decl_signature` so
/// generic params on a method's owning type (`Array<T>::add`'s `T`)
/// render as the concrete instantiation (`String`) instead of the
/// declared param name.
pub(super) struct RenderCtx<'a> {
    pub project: &'a ProjectAnalysis,
    pub subst: &'a FxHashMap<Symbol, TypeId>,
}

/// Substitution-aware variant of [`render_type_ref`]. When `ctx` is
/// `Some` and the `TypeRef` is a bare generic-param ident (no
/// qualifier, no type args) whose symbol is keyed in `ctx.subst`,
/// render via the project's `display_type` on the substituted TypeId
/// instead of emitting the literal param name.
///
/// Nullability handling: when `tr.optional` is true and the
/// substituted TypeId isn't already nullable, append `?` (or
/// ` | null` for a union — `display_type` formats unions without an
/// outer `?` suffix). When the substituted TypeId is already
/// nullable, the rendered form already carries the marker — leave it
/// alone.
pub(super) fn render_type_ref_with_subst(
    hir: &Hir,
    symbols: &SymbolTable,
    type_ref: Idx<greycat_analyzer_hir::types::TypeRef>,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
    let tr = &hir.type_refs[type_ref];
    if let Some(ctx) = ctx
        && tr.qualifier.is_empty()
        && tr.params.is_empty()
        && let Some(&subst_ty) = ctx.subst.get(&hir.idents[tr.name].symbol)
    {
        let rendered = ctx.project.display_type(subst_ty).to_string();
        if tr.optional {
            let arena_ty = ctx.project.arena().get(subst_ty);
            if !arena_ty.nullable {
                return match &arena_ty.kind {
                    TypeKind::Union { .. } => format!("{rendered} | null"),
                    _ => format!("{rendered}?"),
                };
            }
        }
        return rendered;
    }
    let mut out = String::new();
    for q in tr.qualifier.iter() {
        out.push_str(&symbols[hir.idents[*q].symbol]);
        out.push_str("::");
    }
    out.push_str(&symbols[hir.idents[tr.name].symbol]);
    if !tr.params.is_empty() {
        out.push('<');
        for (i, p) in tr.params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&render_type_ref_with_subst(hir, symbols, *p, ctx));
        }
        out.push('>');
    }
    if tr.optional {
        out.push('?');
    }
    out
}

/// Find the receiver's `TypeId` for a property ident bound through
/// member resolution. Walks `hir.exprs` for an `Expr::Member` /
/// `Expr::Arrow` whose `property` is this ident, then reads the
/// receiver expr's settled type from `analysis.expr_types`. Returns
/// `None` when the property isn't carried by a member/arrow expr
/// (e.g. `Type::method` static dispatch — handled elsewhere) or when
/// the receiver's type didn't settle.
fn receiver_ty_for_property(
    hir: &Hir,
    analysis: &AnalysisResult,
    property: Idx<Ident>,
) -> Option<TypeId> {
    let receiver_id = hir.exprs.iter().find_map(|(_, e)| match e {
        Expr::Member(m) | Expr::Arrow(m) if m.property.ident() == property => Some(m.receiver),
        _ => None,
    })?;
    analysis.expr_types.get(&receiver_id).copied()
}

/// Best-effort module label for a foreign URI. Strips trailing `.gcl`
/// off the file name so `file:///proj/lib/std/core.gcl` renders as
/// `core` in the provenance footnote. Falls back to the URI string
/// when path parsing fails.
pub(super) fn module_label_for_uri(uri: &Uri) -> String {
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

fn short_expr_label(hir: &Hir, symbols: &SymbolTable, expr: &Expr) -> String {
    match expr {
        Expr::Ident { name: idx, .. } => symbols[hir.idents[*idx].symbol].to_string(),
        Expr::Literal(_) => "literal".into(),
        Expr::String(_) => "string".into(),
        Expr::Call(_) => "call".into(),
        Expr::Binary(_) => "expression".into(),
        Expr::Unary(_) => "expression".into(),
        Expr::Member(m) | Expr::Arrow(m) => {
            symbols[hir.idents[m.property.ident()].symbol].to_string()
        }
        Expr::Static(s) => symbols[hir.idents[s.property.ident()].symbol].to_string(),
        _ => "expression".into(),
    }
}
