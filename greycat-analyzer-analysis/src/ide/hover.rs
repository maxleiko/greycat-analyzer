//! Hover handlers — single-file + project-aware variants, plus the
//! markdown builders that wrap a rendered signature with doc / member /
//! provenance prose. The signature renderers themselves live in
//! [`crate::ide::render`] so completion and signature_help can reach
//! them without going through this module.
//!
//! Returns the IDE-shape [`Hover`] ADT; the LSP server's
//! `capabilities/hover.rs` converts to `lsp_types::Hover` at the wire
//! boundary, and the wasm crate's `Project::hover` returns the same
//! ADT to JS unchanged.

use std::ops::Range as ByteRange;

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::lsp_types::{Position, Uri};
use greycat_analyzer_core::{SourceEncoding, SourceManager, SymbolTable, TypeArena, TypeId};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, Ident, Stmt, TypeAttr};
use greycat_analyzer_syntax::cst::{ancestors, node_at_offset};
use greycat_analyzer_syntax::tree_sitter;

use crate::analyzer::{AnalysisResult, MemberDef};
use crate::conv::position_to_byte;
use crate::ide::render::{
    RenderCtx, decl_doc, decl_modifier_annotations, module_label_for_uri, push_annotations,
    render_decl_signature, render_type_decl_with_body, render_type_ref, render_type_ref_with_subst,
};
use crate::ide::types::Range as IdeRange;
use crate::project::ProjectAnalysis;
use crate::resolver::{Definition, Resolutions, resolve};
use crate::well_known::DeclRegistry;

/// IDE-shape hover result: markdown body + the source byte-range the
/// hover applies to, already projected into `(line, character)`
/// coordinates so JS / Monaco consumers don't have to re-resolve.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct Hover {
    pub range: IdeRange,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub markdown: String,
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
/// to the in-module-only [`hover_inner`] when the cache is empty.
#[allow(clippy::too_many_arguments)]
pub fn hover_with_project(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    uri: &Uri,
    project: &ProjectAnalysis,
    manager: &SourceManager,
    encoding: SourceEncoding,
) -> Option<Hover> {
    if let Some(module) = project.module(uri) {
        let byte = position_to_byte(text, pos, encoding);
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
                    encoding,
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
                            encoding,
                        ));
                    }
                }
            }
            // Local binder declarator inside a fn body — `Stmt::Var`,
            // `Stmt::For` C-style init, or `Stmt::ForIn` params. None
            // of these live in `module.decls`, and the resolver only
            // inserts their bindings into scope without registering a
            // `Resolutions::uses` entry for the declarator itself, so
            // every other hover branch misses.
            if let Some(markdown) =
                local_binder_hover(&module.hir, project, &module.analysis, ident_idx)
            {
                return Some(hover_from_markdown(
                    markdown,
                    ident.byte_range.clone(),
                    text,
                    encoding,
                ));
            }
            // TypeAttr-defining ident (cursor on the `path` in
            // `private path: String;` inside a type body). Same hover
            // shape as the object-construction site so both ends of
            // the attr binding render consistently. Find the
            // enclosing `Decl::Type` so the provenance footer can
            // name it (`module::Type`).
            if let Some((attr_idx, attr)) = module
                .hir
                .type_attrs
                .iter()
                .find(|(_, a)| module.hir.idents[a.name].byte_range == node.byte_range())
            {
                let owner_name = module.hir.decls.iter().find_map(|(_, decl)| match decl {
                    Decl::Type(td) if td.attrs.contains(&attr_idx) => {
                        Some(&project.symbols()[module.hir.idents[td.name].symbol])
                    }
                    _ => None,
                });
                let provenance = match owner_name {
                    Some(ty_name) => format!("{}::{}", module_label_for_uri(uri), ty_name),
                    None => module_label_for_uri(uri),
                };
                let markdown = object_field_hover_markdown(
                    &module.hir,
                    project.symbols(),
                    attr,
                    &module.hir.idents[attr.name],
                    &provenance,
                );
                return Some(hover_from_markdown(
                    markdown,
                    module.hir.idents[attr.name].byte_range.clone(),
                    text,
                    encoding,
                ));
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
                let mut label = format!(
                    "{}: {}",
                    short_expr_label(&module.hir, project.symbols(), expr),
                    project.display_type(*ty),
                );
                // P-erasure honesty: show the runtime-erased shape for a
                // generic-fn result the runtime erases (see `crate::erasure`).
                if let Some(rt) = module.analysis.expr_runtime_types.get(&expr_id) {
                    label.push_str(&format!(
                        "\n// runtime: {} (GreyCat erases function generics to any?)",
                        project.display_type(*rt),
                    ));
                }
                return Some(hover_from_markdown(wrap_code(&label), r, text, encoding));
            }
        }
        return None;
    }
    // Cache miss — fall back to in-module-only hover.
    hover_inner(text, lib, root, pos, encoding)
}

#[derive(Copy, Clone)]
struct HoverProjectCtx<'a> {
    project: &'a ProjectAnalysis,
    #[allow(dead_code)] // reserved for future cross-module hover content
    manager: &'a SourceManager,
}

fn hover_inner(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
    pos: Position,
    encoding: SourceEncoding,
) -> Option<Hover> {
    let byte = position_to_byte(text, pos, encoding);
    let node = node_at_offset(root, byte)?;
    if !node.is_named() {
        return None;
    }

    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);
    let (arena, decl_registry, analysis) = crate::analyzer::analyze(&hir, &resolutions, &symbols);

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
                encoding,
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
                        encoding,
                    ));
                }
            }
        }
        // Local binder declarator — mirror of the cached path's branch.
        if let Some(markdown) = local_binder_hover_inmodule(
            &hir,
            &symbols,
            &arena,
            &decl_registry,
            &analysis,
            ident_idx,
        ) {
            return Some(hover_from_markdown(
                markdown,
                ident.byte_range.clone(),
                text,
                encoding,
            ));
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
                crate::project::display_type(&arena, &decl_registry, &symbols, *ty),
            );
            return Some(hover_from_markdown(wrap_code(&label), r, text, encoding));
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
    decl_registry: &DeclRegistry,
    ident_idx: Idx<Ident>,
    ident: &Ident,
    project: Option<HoverProjectCtx<'_>>,
) -> Option<String> {
    let ident_name = &symbols[ident.symbol];
    // Object-expression field name (`name` in `Foo { name: value }`).
    // Recorded by the analyzer against the declaring attr — which
    // may live on a *supertype* whose home module is a different
    // file. The binding carries the declaring type's `ItemId`, so the
    // provenance footer points at the chain origin (`module::Type`)
    // rather than just the file: hovering an inherited field on a
    // `Derived { ... }` call site reveals it came from `Base`.
    if let Some(ctx) = project
        && let Some(binding) = analysis.object_field_lookup(ident_idx)
        && let Some(home_uri) = ctx
            .project
            .index
            .module_names
            .get(&binding.declaring_type.module)
            .cloned()
        && let Some(fmod) = ctx.project.module(&home_uri)
        && (binding.attr.into_raw() as usize) < fmod.hir.type_attrs.len()
    {
        let attr = &fmod.hir.type_attrs[binding.attr];
        let provenance = format!(
            "{}::{}",
            module_label_for_uri(&home_uri),
            &symbols[binding.declaring_type.name],
        );
        return Some(object_field_hover_markdown(
            &fmod.hir,
            symbols,
            attr,
            ident,
            &provenance,
        ));
    }
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
                let mut body = format!(
                    "{}: {}",
                    ident_name,
                    crate::project::display_type(arena, decl_registry, symbols, *ty),
                );
                // P-erasure honesty: when the binding holds a generic-fn
                // result the runtime erases, show the erased shape too —
                // the analyzer's type is more specific than what the
                // runtime actually has (see `crate::erasure`).
                if let Some(rt) = analysis.def_runtime_types.get(&name) {
                    body.push_str(&format!(
                        "\n// runtime: {} (GreyCat erases function generics to any?)",
                        crate::project::display_type(arena, decl_registry, symbols, *rt),
                    ));
                }
                wrap_code(&body)
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
    member: MemberDef,
    ident: &Ident,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
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

/// Hover markdown for an object-expression field name. Always
/// includes modifiers (`private` / `static`), declared type, doc, and
/// a `defined in <module>` footnote — useful even when the attr's
/// home module is the same as the construction site, because the
/// type body can be far from the constructor.
fn object_field_hover_markdown(
    hir: &Hir,
    symbols: &SymbolTable,
    attr: &TypeAttr,
    ident: &Ident,
    provenance: &str,
) -> String {
    let ty_str = attr
        .ty
        .map(|t| render_type_ref(hir, symbols, t))
        .unwrap_or_else(|| "any".into());
    let mut signature = String::new();
    if attr.modifiers.private {
        signature.push_str("private ");
    }
    if attr.modifiers.static_ {
        signature.push_str("static ");
    }
    signature.push_str(&symbols[ident.symbol]);
    signature.push_str(": ");
    signature.push_str(&ty_str);

    let mut out = String::new();
    out.push_str(&wrap_code(&signature));
    out.push('\n');
    push_doc_section(&mut out, attr.doc.as_deref());
    out.push_str("\n*defined in `");
    out.push_str(provenance);
    out.push_str("`*");
    out
}

// P15.x
/// Cross-module variant of [`member_hover_markdown`]. Reads
/// the foreign HIR for the attr / method and appends an italic
/// `*defined in `<module>`*` footnote.
fn foreign_member_hover_markdown(
    foreign_hir: &Hir,
    symbols: &SymbolTable,
    member: &MemberDef,
    ident: &Ident,
    provenance: &str,
    ctx: Option<&RenderCtx<'_>>,
) -> String {
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
    // `Decl::Type` gets a multi-line render that inlines up to 5
    // attrs in a `{ … }` body so the reader sees the shape without
    // a goto-def. Native types fall back to the single-line form
    // since they have no `.gcl` body to peek at. Every other decl
    // kind keeps the existing one-line signature.
    //
    // Pragmas / annotations live above the signature inside the
    // same code block — hover is the only surface that wants the
    // full source-form rendering. The signature renderers
    // intentionally don't emit them (completion `detail` strings
    // get flattened to a single line and would be buried).
    let mut signature = String::new();
    push_annotations(&mut signature, symbols, decl_modifier_annotations(decl));
    let body = match decl {
        Decl::Type(td) => render_type_decl_with_body(hir, symbols, td),
        _ => render_decl_signature(hir, symbols, decl, ctx),
    };
    signature.push_str(&body);
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

/// Render a hover string for a cursor on the declaring ident of a
/// local binder — `Stmt::Var.name`, `Stmt::For.init_name`, or any
/// `Stmt::ForIn` param. Returns `None` when the ident isn't a local
/// binder or when its inferred type hasn't settled.
fn local_binder_hover(
    hir: &Hir,
    project: &ProjectAnalysis,
    analysis: &AnalysisResult,
    ident_idx: Idx<Ident>,
) -> Option<String> {
    let (name, has_var_keyword) = local_binder_for(hir, ident_idx)?;
    let ty = analysis.def_types.get(&name).copied()?;
    let prefix = if has_var_keyword { "var " } else { "" };
    let name_str = &project.symbols()[hir.idents[name].symbol];
    Some(wrap_code(&format!(
        "{}{}: {}",
        prefix,
        name_str,
        project.display_type(ty),
    )))
}

fn local_binder_hover_inmodule(
    hir: &Hir,
    symbols: &SymbolTable,
    arena: &TypeArena,
    decl_registry: &DeclRegistry,
    analysis: &AnalysisResult,
    ident_idx: Idx<Ident>,
) -> Option<String> {
    let (name, has_var_keyword) = local_binder_for(hir, ident_idx)?;
    let ty = analysis.def_types.get(&name).copied()?;
    let prefix = if has_var_keyword { "var " } else { "" };
    let name_str = &symbols[hir.idents[name].symbol];
    Some(wrap_code(&format!(
        "{}{}: {}",
        prefix,
        name_str,
        crate::project::display_type(arena, decl_registry, symbols, ty),
    )))
}

/// Returns `Some((name_ident, has_var_keyword))` when `ident_idx` is
/// a local binder declarator. The `has_var_keyword` flag mirrors the
/// source: `var`-introduced binders (`Stmt::Var`, `Stmt::For` C-style
/// init) render with a leading `var`; `Stmt::ForIn` params do not.
fn local_binder_for(hir: &Hir, ident_idx: Idx<Ident>) -> Option<(Idx<Ident>, bool)> {
    hir.stmts.iter().find_map(|(_, stmt)| match stmt {
        Stmt::Var(lv) if lv.name == ident_idx => Some((lv.name, true)),
        Stmt::For(fs) if fs.init_name == Some(ident_idx) => Some((ident_idx, true)),
        Stmt::ForIn(fis) => fis
            .params
            .iter()
            .find(|p| p.name == ident_idx)
            .map(|p| (p.name, false)),
        _ => None,
    })
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

fn hover_from_markdown(
    markdown: String,
    range: ByteRange<usize>,
    text: &str,
    encoding: SourceEncoding,
) -> Hover {
    Hover {
        range: IdeRange::from_byte_range(text, &range, encoding),
        markdown,
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
