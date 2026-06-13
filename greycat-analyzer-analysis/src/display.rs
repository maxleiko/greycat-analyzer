//! Type display (`TypeKind::Type` / `TypeKind::Generic` rendering).
//!
//! Lives in the analysis crate because rendering decl-keyed types
//! needs the project's [`SymbolTable`] to recover the source name
//! from the `ItemId`'s `name` symbol. The bare core [`TypeArena`]
//! does not own a symbol table — see `greycat_analyzer_core::types`
//! for the rationale.

use greycat_analyzer_core::{ItemId, SymbolTable, TypeArena, TypeId, TypeKind, lsp_types::Uri};
use greycat_analyzer_hir::DeclRegistry;

use crate::{index::ProjectIndex, project::ProjectAnalysis};

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
        TypeKind::Type(d) => {
            let name = &symbols[d.name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::Generic { tpl, args } => {
            let name = &symbols[tpl.name];
            let parts: Box<[String]> = args
                .iter()
                .map(|a| display_fqn(arena, symbols, *a, home_lib))
                .collect();
            format!(
                "{}::{name}<{}>",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
                parts.join(", ")
            )
        }
        // Unresolved type-refs degrade to `core::any` in display so
        // diagnostics quoting full type structures don't pretend the
        // unbound name resolved to something it didn't. The arena
        // still carries `Unresolved { name, byte_range }` for goto /
        // hover anchoring; only the printed form is degraded.
        TypeKind::Unresolved { .. } => "core::any".to_string(),
        TypeKind::GenericParam(name) => symbols[*name].to_string(),
        TypeKind::Lambda { params, ret } => {
            let parts: Box<[String]> = params
                .iter()
                .map(|p| display_fqn(arena, symbols, *p, home_lib))
                .collect();
            match ret {
                Some(r) => format!(
                    "fn({}): {}",
                    parts.join(", "),
                    display_fqn(arena, symbols, *r, home_lib)
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
                .map(|a| display_fqn(arena, symbols, *a, home_lib))
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
        TypeKind::TypeOf(inner) => {
            format!("typeof {}", display_fqn(arena, symbols, *inner, home_lib))
        }
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        s.push('?');
    }
    s
}

/// `Display`-implementing wrapper returned by
/// [`ProjectAnalysis::display_type`]. Prefixes `<module>::` whenever
/// the bare decl name is ambiguous within the project (≥2 modules
/// export it). When the name is unique, output matches the
/// registry-aware [`display_type`] byte-for-byte.
pub struct ProjectTypeDisplay<'a> {
    pub(crate) project: &'a ProjectAnalysis,
    pub(crate) id: TypeId,
}

impl std::fmt::Display for ProjectTypeDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_qualified(f, self.project.arena(), &self.project.index, self.id)
    }
}

/// Index-aware [`Display`] wrapper for a [`TypeId`]. Renders the same
/// way as [`ProjectAnalysis::display_type`] — `module::Name` when the
/// bare decl name is ambiguous within the project, bare otherwise — but
/// without requiring a full `ProjectAnalysis`. Lets per-module
/// consumers (the lints invoked from `run_typed_lints_for_module`)
/// emit ambiguity-qualified type names in their diagnostic messages so
/// downstream consumers (the `infer-return-type` quickfix) paste the
/// qualified form back into source.
pub fn display_type_qualified<'a>(
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    id: TypeId,
) -> QualifiedTypeDisplay<'a> {
    QualifiedTypeDisplay { arena, index, id }
}

pub struct QualifiedTypeDisplay<'a> {
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    id: TypeId,
}

impl std::fmt::Display for QualifiedTypeDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_qualified(f, self.arena, self.index, self.id)
    }
}

fn write_type_qualified(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    id: TypeId,
) -> std::fmt::Result {
    let ty = arena.get(id);
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Type(d) => write_decl_qualified(f, index, *d)?,
        TypeKind::Generic { tpl, args } => {
            write_decl_qualified(f, index, *tpl)?;
            write_args_qualified(f, arena, index, args)?;
        }
        // A type-ref that didn't resolve flows through as opaque
        // `any?` — print the degraded form so callers see the honest
        // shape (the `?` is added by the nullable postfix below
        // because `arena.unresolved()` builds with nullable: true).
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam(name) => f.write_str(&index.symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_qualified(f, arena, index, *p)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_qualified(f, arena, index, *r)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_qualified(f, arena, index, *a)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_qualified(f, arena, index, *inner)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

fn write_args_qualified(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    args: &[TypeId],
) -> std::fmt::Result {
    f.write_str("<")?;
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_type_qualified(f, arena, index, *a)?;
    }
    f.write_str(">")
}

/// Registry-aware [`Display`] wrapper for a [`TypeId`]. Consults
/// `decl_registry` to recover decl names for `Type(d)` / `Generic{decl,
/// args}` so error messages and lint diagnostics surface the real
/// `Foo` / `Map<int, String>`. No module-qualification logic — use
/// [`ProjectAnalysis::display_type`] when ambiguity disambiguation is
/// needed.
pub fn display_type<'a>(
    arena: &'a TypeArena,
    decl_registry: &'a DeclRegistry,
    symbols: &'a SymbolTable,
    id: TypeId,
) -> TypeWithDecls<'a> {
    TypeWithDecls {
        arena,
        decl_registry,
        symbols,
        id,
    }
}

/// Sibling of [`display_type`] that qualifies decls *only when bare
/// lookup from `current_uri` wouldn't reach them* — i.e. the bare form
/// is ambiguous, private cross-module, or absent. Used by error
/// messages that name foreign decls (e.g. cross-module
/// `argument-type-mismatch`) so the rendered text is itself a valid
/// reference from the diagnostic's source position. Decls reachable
/// bare from `current_uri` stay bare to avoid `core::Map` noise.
pub fn display_type_for_module<'a>(
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    decl_registry: &'a DeclRegistry,
    id: TypeId,
    current_uri: Option<&'a Uri>,
) -> TypeForModule<'a> {
    TypeForModule {
        arena,
        index,
        decl_registry,
        id,
        current_uri,
    }
}

pub struct TypeForModule<'a> {
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    decl_registry: &'a DeclRegistry,
    id: TypeId,
    /// When `Some(uri)`, qualify any decl that bare lookup from `uri`
    /// wouldn't reach (private cross-module, ambiguous across two
    /// public modules, or shadowed). When `None`, qualification falls
    /// back to the project-wide `locate_decl(...).len() > 1`
    /// ambiguity heuristic — same as [`write_type_qualified`].
    current_uri: Option<&'a Uri>,
}

impl std::fmt::Display for TypeForModule<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_for_module(
            f,
            self.arena,
            self.index,
            self.decl_registry,
            self.id,
            self.current_uri,
        )
    }
}

fn write_type_for_module(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    id: TypeId,
    current_uri: Option<&Uri>,
) -> std::fmt::Result {
    use greycat_analyzer_core::TypeKind;
    let ty = arena.get(id);
    let decl_name = |d: ItemId, f: &mut std::fmt::Formatter<'_>| -> std::fmt::Result {
        // Builtin primitives are universally in scope, so they always
        // render bare -- a `core::int` in a diagnostic is just noise.
        // Qualify any other decl iff bare-name lookup from `current_uri`
        // wouldn't bind to this exact decl (the bare form would miss or
        // bind to a different decl).
        let is_prim = arena.builtins().is_some_and(|b| b.is_primitive(d));
        let needs_qual = !is_prim
            && match index.resolve_item(decl_registry, current_uri, d.name) {
                Some(found) => found != d,
                None => true,
            };
        if needs_qual {
            f.write_str(&index.symbols[d.module])?;
            f.write_str("::")?;
        }
        f.write_str(&index.symbols[d.name])
    };
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Type(d) => decl_name(*d, f)?,
        TypeKind::Generic { tpl, args } => {
            decl_name(*tpl, f)?;
            f.write_str("<")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *a, current_uri)?;
            }
            f.write_str(">")?;
        }
        // See `write_type_qualified`: degrade unresolved type-refs to
        // `any?` in display so error messages don't pretend the name
        // resolved to something it didn't.
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam(name) => f.write_str(&index.symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *p, current_uri)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_for_module(f, arena, index, decl_registry, *r, current_uri)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *a, current_uri)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_for_module(f, arena, index, decl_registry, *inner, current_uri)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

pub struct TypeWithDecls<'a> {
    arena: &'a TypeArena,
    decl_registry: &'a DeclRegistry,
    symbols: &'a SymbolTable,
    id: TypeId,
}

impl std::fmt::Display for TypeWithDecls<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_with_decls(f, self.arena, self.decl_registry, self.symbols, self.id)
    }
}

fn write_type_with_decls(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    _decl_registry: &DeclRegistry,
    symbols: &SymbolTable,
    id: TypeId,
) -> std::fmt::Result {
    use greycat_analyzer_core::TypeKind;
    let ty = arena.get(id);
    let decl_name = |d: ItemId, f: &mut std::fmt::Formatter<'_>| -> std::fmt::Result {
        f.write_str(&symbols[d.name])
    };
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Type(d) => decl_name(*d, f)?,
        TypeKind::Generic { tpl, args } => {
            decl_name(*tpl, f)?;
            f.write_str("<")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *a)?;
            }
            f.write_str(">")?;
        }
        // See `write_type_qualified`: degrade unresolved type-refs to
        // `any?` in display so error messages don't pretend the name
        // resolved to something it didn't.
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam(name) => f.write_str(&symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *p)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_with_decls(f, arena, _decl_registry, symbols, *r)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *a)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_with_decls(f, arena, _decl_registry, symbols, *inner)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

fn write_decl_qualified(
    f: &mut std::fmt::Formatter<'_>,
    index: &ProjectIndex,
    decl: ItemId,
) -> std::fmt::Result {
    // Two same-named items in different modules → render with the
    // `module::` qualifier; otherwise the bare name is unambiguous.
    if index.locate_decl(decl.name).len() > 1 {
        f.write_str(&index.symbols[decl.module])?;
        f.write_str("::")?;
    }
    f.write_str(&index.symbols[decl.name])
}
