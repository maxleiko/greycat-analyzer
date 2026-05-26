//! Borrow-only HIR view shapes — projection of `ProjectAnalysis` into
//! a human-readable tree for the `greycat-analyzer hir` subcommand.
//!
//! Hard design rule: the structs here own **no** `String` fields. Every
//! textual slot is `&'a str` (borrowing from the source text, the
//! `SymbolTable`, or HIR-side strings like docs) or `Cow<'a, str>` for
//! the one slot that has to be computed on the fly (FQN displays — see
//! `display_fqn` which returns `String`). This keeps the view a true
//! projection: it can't outlive the analysis it was derived from, and
//! it doesn't double the analysis's memory footprint.

use std::borrow::Cow;
use std::ops::Range;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Project<'a> {
    pub root: &'a str,
    pub modules: Vec<Module<'a>>,
    /// Deduplicated, project-wide list of generic instantiations
    /// observed in the type arena (e.g. `Array<core::int>`,
    /// `Map<core::String, project::Foo>`). Sourced from
    /// `TypeKind::Generic { decl, args }` entries whose decl resolved
    /// through `resolve_decl_handle`; unresolved generics are skipped.
    pub monomorphizations: Vec<Monomorphization<'a>>,
}

#[derive(Debug, Serialize)]
pub struct Module<'a> {
    pub name: &'a str,
    pub lib: &'a str,
    pub uri: &'a str,
    pub rel_path: &'a str,
    pub types: Vec<TypeView<'a>>,
    pub fns: Vec<FnView<'a>>,
    pub enums: Vec<EnumView<'a>>,
    pub vars: Vec<VarView<'a>>,
    pub pragmas: Vec<PragmaView<'a>>,
    pub resolutions: Vec<ResolutionView<'a>>,
}

#[derive(Debug, Serialize)]
pub struct TypeView<'a> {
    pub name: &'a str,
    pub id: u32,
    /// `Some` when the type is registered in the project arena
    /// (`TypeArena.alloc_type` issued an id for it). Always set for
    /// non-pragma user types in well-formed projects.
    pub type_id: Option<u32>,
    pub modifiers: ModifiersView<'a>,
    pub generics: Vec<&'a str>,
    /// Full `extends` chain, leaf-first. Includes the immediate parent
    /// at index 0; the chain stops at the first link that doesn't
    /// resolve (or at the runtime's depth ceiling, matching
    /// `ProjectIndex::MAX_INHERITANCE_DEPTH`).
    pub extends_chain: Vec<ExtendsLink<'a>>,
    pub attrs: Vec<AttrView<'a>>,
    pub methods: Vec<FnView<'a>>,
    pub doc: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ExtendsLink<'a> {
    pub name: &'a str,
    /// Module that declares the parent type (`std`, `project`, …) — the
    /// home library stem of the resolved decl. `None` when the chain
    /// reaches a name whose home module we couldn't locate.
    pub lib: Option<&'a str>,
    /// `Some` when the parent's instantiated shape is registered in the
    /// project arena (`Sub extends Base<int>` → `core::Base<core::int>`).
    /// `None` when the supertype didn't resolve through signature
    /// lowering — capabilities fall back to the symbol-only walk.
    pub instantiated: Option<Cow<'a, str>>,
}

#[derive(Debug, Serialize)]
pub struct AttrView<'a> {
    pub name: &'a str,
    pub id: u32,
    pub modifiers: ModifiersView<'a>,
    /// Resolved canonical type (`core::int`, `project::Foo`,
    /// `core::Array<core::String>`, …). `None` when the attr has no
    /// declared type and signature lowering didn't infer one.
    pub ty: Option<Cow<'a, str>>,
    pub has_init: bool,
    pub doc: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct FnView<'a> {
    pub name: &'a str,
    pub id: u32,
    pub modifiers: ModifiersView<'a>,
    pub generics: Vec<&'a str>,
    pub params: Vec<ParamView<'a>>,
    /// Resolved canonical return type. `None` when no return type was
    /// declared AND signature lowering didn't capture one.
    pub return_ty: Option<Cow<'a, str>>,
    pub has_body: bool,
    pub doc: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ParamView<'a> {
    pub name: &'a str,
    pub ty: Option<Cow<'a, str>>,
}

#[derive(Debug, Serialize)]
pub struct EnumView<'a> {
    pub name: &'a str,
    pub id: u32,
    pub modifiers: ModifiersView<'a>,
    /// Enum entries (called "fields" in GreyCat — the grammar's
    /// `enum_field` node).
    pub fields: Vec<EnumFieldView<'a>>,
    pub doc: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct EnumFieldView<'a> {
    pub name: &'a str,
    pub has_value: bool,
}

#[derive(Debug, Serialize)]
pub struct VarView<'a> {
    pub name: &'a str,
    pub id: u32,
    pub modifiers: ModifiersView<'a>,
    pub ty: Option<Cow<'a, str>>,
    pub initializer: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct PragmaView<'a> {
    pub name: &'a str,
    /// Source slice of each argument expression, in declaration order.
    pub args: Vec<&'a str>,
}

#[derive(Debug, Serialize)]
pub struct ModifiersView<'a> {
    pub private: bool,
    pub static_: bool,
    pub abstract_: bool,
    pub native: bool,
    /// Annotation names with their string-literal args (e.g.
    /// `@expose("renamed")` → `[("expose", ["renamed"])]`).
    pub annotations: Vec<AnnotationView<'a>>,
}

#[derive(Debug, Serialize)]
pub struct AnnotationView<'a> {
    pub name: &'a str,
    /// Pre-rendered argument values, one entry per source arg. The
    /// HIR carries typed values (`AnnotationArg::Int(42)`,
    /// `AnnotationArg::String(sym)`, `AnnotationArg::Path { chain
    /// }`, …); the dump renders each variant in a stable
    /// human-readable form (`"foo"`, `42`, `null`,
    /// `DurationUnit::milliseconds`, …).
    pub args: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Monomorphization<'a> {
    pub display: Cow<'a, str>,
    pub args: Vec<Cow<'a, str>>,
}

#[derive(Debug, Serialize)]
pub struct ResolutionView<'a> {
    /// Source text of the ident-use site (slice into the module's
    /// document text).
    pub source: &'a str,
    pub byte_range: Range<usize>,
    /// Fully-qualified name of the decl this ident binds to
    /// (`project::Foo`, `core::Array`, `core::node`, …). `None` for
    /// unresolved idents, locals, params, and generic params — those
    /// don't have an FQN.
    pub binds_to: Option<Cow<'a, str>>,
    /// Coarse classification: `decl` / `local` / `param` / `generic` /
    /// `project-decl` / `project` / `unresolved`.
    pub kind: &'static str,
}
