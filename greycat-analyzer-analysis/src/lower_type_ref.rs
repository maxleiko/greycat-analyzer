//! Single source of truth for lowering a syntactic [`TypeRef`] to an
//! interned [`TypeId`]. The body walker, signature lowering, and
//! type-relation validation each implement [`TypeRefLowering`] to supply
//! the few things that vary by stage (local registry, generic scope, the
//! static-generic diagnostic sink). The ladder lives here once so the
//! stages cannot drift and mint divergent shapes for one source token.

use rustc_hash::FxHashMap;

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{Builtins, GenericOwner, Symbol, TypeArena, TypeId};
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, TypeRef};
use greycat_analyzer_hir::{DeclRegistry, Hir};

use crate::index::{Namespace, ProjectIndex};

/// Stage-varying inputs to the shared lowering ladder. The arena is
/// passed alongside (never on the trait) so a `&mut self` impl can hand
/// the core a `&mut TypeArena` it also borrows.
pub(crate) trait TypeRefLowering {
    fn hir(&self) -> &Hir;
    fn index(&self) -> &ProjectIndex;
    fn decl_registry(&self) -> &DeclRegistry;
    fn current_uri(&self) -> Option<&Uri>;

    /// Bare name already minted into the working arena by an earlier
    /// stage. Body walk: the module's `out.registry`. Validation: the
    /// project `TypeRegistry`. Signature lowering has no such table.
    fn lookup_local(&self, _name: Symbol) -> Option<TypeId> {
        None
    }

    /// Bare name that is a generic param in scope -> its owner.
    fn lookup_generic(&self, _name: Symbol) -> Option<GenericOwner> {
        None
    }

    /// Arity of a bare generic type name in raw form (`Tensor` ==
    /// `Tensor<any?, any?>`); `None` for non-generic / unknown.
    fn generic_arity_for(&self, name: Symbol) -> Option<usize>;

    fn inside_static_fn(&self) -> bool {
        false
    }

    /// Record a type-level generic referenced from a `static` fn body
    /// (drives `generic-in-static-context`). No-op outside the body walk.
    fn note_static_generic_use(&mut self, _idx: Idx<TypeRef>) {}
}

/// Walk a generic-scope stack (innermost last) for `name`.
pub(crate) fn lookup_generic_in(
    stack: &[FxHashMap<Symbol, GenericOwner>],
    name: Symbol,
) -> Option<GenericOwner> {
    stack
        .iter()
        .rev()
        .find_map(|frame| frame.get(&name).copied())
}

/// Raw-form generic arity: a local decl shadows the project index; the
/// first non-private cross-module candidate with non-zero arity wins.
pub(crate) fn generic_arity_for(
    name: Symbol,
    hir: &Hir,
    type_decls: &FxHashMap<Symbol, Idx<Decl>>,
    index: &ProjectIndex,
) -> Option<usize> {
    if let Some(decl_id) = type_decls.get(&name)
        && let Decl::Type(td) = &hir.decls[*decl_id]
        && !td.generics.is_empty()
    {
        return Some(td.generics.len());
    }
    for (uri, decl) in index.locate_decl_in_ns(name, Namespace::Type) {
        if index.is_decl_private(uri, decl) {
            continue;
        }
        let Some(item) = index.item_id_for(uri, name) else {
            continue;
        };
        let arity = index.type_members.get(&item)?.generics.len();
        if arity > 0 {
            return Some(arity);
        }
    }
    None
}

/// Lower `idx` to a `TypeId`, minting into `arena`. The single ladder
/// every stage shares; `env` supplies the stage-varying lookups.
pub(crate) fn lower_type_ref_with<E: TypeRefLowering>(
    env: &mut E,
    arena: &mut TypeArena,
    idx: Idx<TypeRef>,
) -> TypeId {
    // Clone so the `env` borrow is released before any `&mut env` call
    // (the static-generic sink) further down the ladder.
    let tr = env.hir().type_refs[idx].clone();
    let base = if !tr.qualifier.is_empty() {
        lower_qualified_base(env, arena, &tr)
    } else {
        let name = env.hir().idents[tr.name].symbol;
        let builtin = primitive_or_special(&env.index().symbols[name], arena);
        match builtin {
            Some(b) => b,
            None => lower_bare_name(env, arena, &tr, idx, name),
        }
    };
    wrap_marker(arena, base, &tr)
}

/// Seeded primitives plus `any` / `null`; `None` for any other name.
fn primitive_or_special(name: &str, arena: &mut TypeArena) -> Option<TypeId> {
    Some(match name {
        "bool" => arena.builtin(Builtins::BOOL),
        "int" => arena.builtin(Builtins::INT),
        "float" => arena.builtin(Builtins::FLOAT),
        "char" => arena.builtin(Builtins::CHAR),
        "String" => arena.builtin(Builtins::STRING),
        "time" => arena.builtin(Builtins::TIME),
        "duration" => arena.builtin(Builtins::DURATION),
        "geo" => arena.builtin(Builtins::GEO),
        "any" => arena.any(),
        "null" => arena.null(),
        _ => return None,
    })
}

/// Non-primitive bare name: generic instantiation, generic param,
/// raw-form generic, local/registered type, enum, concrete handle, or
/// `Unresolved`. `resolved` is the shared decl handle for this name.
fn lower_bare_name<E: TypeRefLowering>(
    env: &mut E,
    arena: &mut TypeArena,
    tr: &TypeRef,
    idx: Idx<TypeRef>,
    name: Symbol,
) -> TypeId {
    let span = (tr.byte_range.start, tr.byte_range.end);
    let resolved = env
        .index()
        .resolve_item(env.decl_registry(), env.current_uri(), name);

    if !tr.params.is_empty() {
        let args = lower_params(env, arena, &tr.params);
        return match resolved {
            Some(handle) => arena.alloc_generic(handle, args),
            None => arena.unresolved(name, span),
        };
    }
    if let Some(owner) = env.lookup_generic(name) {
        if env.inside_static_fn() && matches!(owner, GenericOwner::Type(_)) {
            env.note_static_generic_use(idx);
        }
        return arena.generic_param(name);
    }
    if let Some(arity) = env.generic_arity_for(name) {
        let any_q = arena.any_nullable();
        let args = vec![any_q; arity];
        return match resolved {
            Some(handle) => arena.alloc_generic(handle, args),
            None => arena.unresolved(name, span),
        };
    }
    if let Some(id) = env.lookup_local(name) {
        return id;
    }
    if let Some(enum_id) = resolved.and_then(|item| env.index().enum_types.get(&item).copied()) {
        return enum_id;
    }
    if let Some(handle) = resolved {
        return arena.alloc_type(handle);
    }
    arena.unresolved(name, span)
}

/// Qualified ref (`b::Foo`): bind to the leaf decl in the named module
/// specifically. Returns the unwrapped base; the caller applies
/// `typeof` / `?`.
fn lower_qualified_base<E: TypeRefLowering>(
    env: &mut E,
    arena: &mut TypeArena,
    tr: &TypeRef,
) -> TypeId {
    let module_seg = *tr
        .qualifier
        .last()
        .expect("lower_qualified_base called with empty qualifier");
    let module_name = env.hir().idents[module_seg].symbol;
    let leaf = env.hir().idents[tr.name].symbol;
    let span = (tr.byte_range.start, tr.byte_range.end);

    // Own the URI so the `env` borrow is released before recursing on
    // params (which can mutate `env` via the static-generic sink).
    let Some(module_uri) = env.index().module_names.get(&module_name).cloned() else {
        return arena.unresolved(leaf, span);
    };

    if !tr.params.is_empty() {
        let args = lower_params(env, arena, &tr.params);
        return match env
            .index()
            .item_id_for(&module_uri, leaf)
            .filter(|item| env.decl_registry().lookup(*item).is_some())
        {
            Some(item) => arena.alloc_generic(item, args),
            None => arena.unresolved(leaf, span),
        };
    }

    match env
        .index()
        .item_id_for(&module_uri, leaf)
        .filter(|item| env.decl_registry().lookup(*item).is_some())
    {
        Some(item) => arena.alloc_type(item),
        None => arena.unresolved(leaf, span),
    }
}

fn lower_params<E: TypeRefLowering>(
    env: &mut E,
    arena: &mut TypeArena,
    params: &[Idx<TypeRef>],
) -> Vec<TypeId> {
    let mut args = Vec::with_capacity(params.len());
    for p in params {
        args.push(lower_type_ref_with(env, arena, *p));
    }
    args
}

/// Apply the `typeof` wrapper then the `?` nullable marker, in that order.
fn wrap_marker(arena: &mut TypeArena, mut base: TypeId, tr: &TypeRef) -> TypeId {
    if tr.typeof_marker {
        base = arena.type_of(base);
    }
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}
