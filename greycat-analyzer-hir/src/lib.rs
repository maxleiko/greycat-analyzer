//! HIR for greycat — typed surface tree built by lowering tree-sitter CST.
//! Shapes are in [`types`]; the lowering walker is in [`lower`].

pub mod arena;
mod decl;
mod hir;
pub mod lower;
pub mod types;

pub use decl::DeclRegistry;
pub use hir::Hir;
pub use lower::{LowerCtx, lower_module};
