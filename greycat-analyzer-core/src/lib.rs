//! Foundation crate for the greycat analyzer: the document / project
//! model plus the type system the higher layers build on.
//!
//! **Document & project graph** — the semantic glue between the
//! [`greycat_analyzer_syntax`] tree-sitter wrapper and the analyzer /
//! LSP / CLI consumers:
//!
//! - [`Document`] — a parsed `.gcl` file with line index, version, and
//!   the tree-sitter [`Tree`].
//! - [`SourceManager`] — keyed by `Uri`, holds every loaded document and
//!   drives recursive `@library` / `@include` loading through a
//!   [`resolver::Context`].
//! - [`module_desc`] — pulls `@library` / `@include` / pragma names out
//!   of a parsed CST without lowering to HIR.
//! - [`diagnostics`] — parse-time diagnostics (ERROR / MISSING nodes)
//!   shaped as `lsp_types::Diagnostic`.
//! - [`resolver`] — `@library` / `@include` path math and the filesystem
//!   [`Context`] trait other crates stub for tests.
//!
//! **Type system** — the interned representation the analyzer mints into:
//!
//! - [`Type`] / [`TypeKind`] — the central type enum (decl-keyed types,
//!   generics, lambda, enum, null / any / never) under a nullable wrapper.
//! - [`TypeArena`] — interns [`Type`]s to `Copy` [`TypeId`]s and owns the
//!   subtyping / castability relations and the canonical [`Builtins`]
//!   identities.
//! - [`ItemId`] / [`Symbol`] / [`SymbolTable`] — interned decl + name
//!   identities shared project-wide.
//! - [`TypeRegistry`], [`InferenceTable`] — per-module type lookup and the
//!   inference foundation.
//!
//! Re-exports `lsp_types` and `greycat_analyzer_syntax` so downstream
//! crates depend on this one and pick up both transitively.

pub mod conv;
pub mod diagnostics;
mod doc;
mod manager;
pub mod module_desc;
pub mod registry;
pub mod resolver;
mod symbols;
mod type_arena;
mod types;

pub use doc::*;
pub use manager::*;
pub use symbols::*;
pub use type_arena::*;
pub use types::*;

/// Re-export `lsp_types`
pub use lsp_types;

/// Re-export the syntax crate so downstream users can reach tree-sitter
/// types and the generated typed-node accessors without a separate dep.
pub use greycat_analyzer_syntax as syntax;
