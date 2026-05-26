//! Capability-shaped analysis services consumed by editor-style
//! clients (LSP server, `lint --fix` CLI). Each module here exposes
//! the analysis half of one user-visible operation, decoupled from
//! `lsp_types` so the same logic is reachable from non-LSP callers.
//!
//! Convention: the LSP layer in `greycat-analyzer-server/src/capabilities/`
//! becomes a thin shape-converter on top of these services. If a new
//! capability handler is doing analysis work (scope walking, member
//! discovery, name binding queries, edit synthesis), the work lives
//! here and the LSP layer does only `byte_range → lsp_types::Range`
//! conversion. See [`rename`] for the reference pattern.

pub mod actions;
pub mod completion;
pub mod diagnostics;
pub mod document_highlights;
pub mod folding_ranges;
pub mod hover;
pub mod inlay_hints;
pub mod quickfix;
pub mod rename;
pub mod render;
pub mod scope;
pub mod signature_help;
pub mod types;
