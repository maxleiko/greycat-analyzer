//! Signature / type-ref rendering for decls, fn signatures, and type
//! references. Consumed by hover, completion, and signature_help on
//! the LSP side; lives here so the LSP capabilities stay shape-only.
//!
//! The substitution-aware variants ([`render_type_ref_with_subst`],
//! [`render_decl_signature`] when `ctx` is `Some`) thread a
//! receiver-instantiation `subst` map so generic params on a method's
//! owning type render as the concrete instantiation (e.g. hovering
//! `arr.add` on `arr: Array<String>` renders `fn add(value: String):
//! null` instead of the declared `fn add(value: T): null`).
//!
//! Pure analysis output: returns plain `String`s, takes no
//! `SourceManager` and no `lsp_types` shapes beyond `Uri` for the
//! cross-module provenance label.

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{Symbol, SymbolTable, TypeId, TypeKind};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, FnDecl, TypeDecl, TypeRef};
use rustc_hash::FxHashMap;

use crate::project::ProjectAnalysis;

/// Receiver-instantiation context for substitution-aware rendering.
/// `subst` maps each generic-param symbol (as declared on the owning
/// type / fn) to its concrete instantiation `TypeId`.
pub struct RenderCtx<'a> {
    pub project: &'a ProjectAnalysis,
    pub subst: &'a FxHashMap<Symbol, TypeId>,
}

pub fn decl_doc(decl: &Decl) -> Option<&str> {
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
/// receiver's concrete args. The free-function / type-decl paths pass
/// `None` and the renderer behaves byte-identically to the unsubst
/// form.
pub fn render_decl_signature(
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

pub fn render_fn_signature(
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

pub fn render_type_ref(hir: &Hir, symbols: &SymbolTable, type_ref: Idx<TypeRef>) -> String {
    render_type_ref_with_subst(hir, symbols, type_ref, None)
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
pub fn render_type_ref_with_subst(
    hir: &Hir,
    symbols: &SymbolTable,
    type_ref: Idx<TypeRef>,
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

/// Best-effort module label for a foreign URI. Strips trailing `.gcl`
/// off the file name so `file:///proj/lib/std/core.gcl` renders as
/// `core` in the provenance footnote. Falls back to the URI string
/// when path parsing fails.
pub fn module_label_for_uri(uri: &Uri) -> String {
    let s = uri.as_str();
    let path_part = s.strip_prefix("file://").unwrap_or(s);
    let last = path_part.rsplit(['/', '\\']).next().unwrap_or(path_part);
    last.strip_suffix(".gcl").unwrap_or(last).to_string()
}
