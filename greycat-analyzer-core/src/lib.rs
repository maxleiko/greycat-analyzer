//! Document model + project graph for greycat.
//!
//! This crate is the "semantic glue" that sits between the
//! [`greycat_analyzer_syntax`] tree-sitter wrapper and the higher-level
//! analyzer / LSP / CLI consumers. It owns:
//!
//! - [`Document`] — a parsed `.gcl` file with line index, version,
//!   and the tree-sitter [`Tree`].
//! - [`SourceManager`] — keyed by `Uri`, holds every loaded
//!   document and drives recursive `@library` / `@include` loading
//!   through a [`resolver::Context`].
//! - [`module_desc`] — pulls `@library` / `@include` / pragma names
//!   out of a parsed CST without lowering to HIR.
//! - [`diagnostics`] — parse-time diagnostics (ERROR / MISSING
//!   nodes) shaped as `lsp_types::Diagnostic`.
//! - [`resolver`] — `@library` / `@include` path math and the
//!   filesystem [`Context`] trait that other crates can stub for
//!   tests.
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
pub mod span;
mod symbols;
mod types;
pub mod cst;

pub use doc::*;
pub use manager::*;
pub use symbols::*;
pub use types::*;

/// Re-export `lsp_types`
pub use lsp_types;

/// Re-export the syntax crate so downstream users can reach tree-sitter
/// types and the generated typed-node accessors without a separate dep.
pub use greycat_analyzer_syntax as syntax;
