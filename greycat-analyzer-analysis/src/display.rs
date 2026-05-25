//! Type display (`TypeKind::Type` / `TypeKind::Generic` rendering).
//!
//! Lives in the analysis crate because rendering decl-keyed types
//! needs the project's [`SymbolTable`] to recover the source name
//! from the `ItemId`'s `name` symbol. The bare core [`TypeArena`]
//! does not own a symbol table — see `greycat_analyzer_core::types`
//! for the rationale.

use greycat_analyzer_core::{SymbolTable, TypeArena, TypeId, TypeKind};

use crate::well_known::DeclRegistry;

/// Fully-qualified-name display, matching the GreyCat canonical
/// printer (e.g. `core::int`, `core::Array<core::int?>`,
/// `project::Foo`).
///
/// `home_lib` resolves a Type/Generic/Enum's home module (e.g. `Foo →
/// "project"`, `node → "core"`). Returning `None` falls back to the
/// `core` library — matches the TS reference's behavior for builtins
/// not in the project decl table.
///
/// Primitives, builtin runtime types, and unresolved names get a
/// `core::` prefix. User types resolve to `<lib>::<Name>` via
/// `home_lib`. Nullability is rendered with the `?` suffix for every
/// kind except `Null` and `Union` (the latter expands an explicit
/// `| null` alt to avoid the suffix visually binding to the last alt).
pub fn display_fqn(
    arena: &TypeArena,
    _decl_registry: &DeclRegistry,
    symbols: &SymbolTable,
    id: TypeId,
    home_lib: &dyn Fn(&str) -> Option<String>,
) -> String {
    let ty = arena.get(id);
    let mut s = match &ty.kind {
        // TS reference's `dump-types` emits the bare null literal as
        // `null`, not `core::null` — match that.
        TypeKind::Null => "null".to_string(),
        TypeKind::Any => "core::any".to_string(),
        TypeKind::Never => "core::never".to_string(),
        TypeKind::Primitive(p) => format!("core::{}", p.name()),
        TypeKind::Type(d) => {
            let name = &symbols[d.name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::Generic { decl, args } => {
            let name = &symbols[decl.name];
            let parts: Box<[String]> = args
                .iter()
                .map(|a| display_fqn(arena, _decl_registry, symbols, *a, home_lib))
                .collect();
            format!(
                "{}::{name}<{}>",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
                parts.join(", ")
            )
        }
        // P35.3 — unresolved name, render verbatim with the same
        // `<lib>::` prefix the rest of the resolver would have used.
        TypeKind::Unresolved { name, .. } => {
            let name = &symbols[*name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::GenericParam { name, .. } => symbols[*name].to_string(),
        TypeKind::Lambda { params, ret } => {
            let parts: Box<[String]> = params
                .iter()
                .map(|p| display_fqn(arena, _decl_registry, symbols, *p, home_lib))
                .collect();
            match ret {
                Some(r) => format!(
                    "fn({}): {}",
                    parts.join(", "),
                    display_fqn(arena, _decl_registry, symbols, *r, home_lib)
                ),
                None => format!("fn({})", parts.join(", ")),
            }
        }
        TypeKind::Enum { name, .. } => {
            let name = &symbols[*name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::Union { alts } => {
            let mut parts: Vec<String> = alts
                .iter()
                .map(|a| display_fqn(arena, _decl_registry, symbols, *a, home_lib))
                .collect();
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                parts.push("null".to_string());
            }
            parts.join(" | ")
        }
        // P-typeof — render the source form `typeof Inner`, mirroring the
        // grammar's `optional("typeof")` prefix on `type_ident`.
        TypeKind::TypeOf(inner) => format!(
            "typeof {}",
            display_fqn(arena, _decl_registry, symbols, *inner, home_lib)
        ),
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        s.push('?');
    }
    s
}
