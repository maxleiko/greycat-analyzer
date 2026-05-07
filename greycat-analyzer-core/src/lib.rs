pub mod module_desc;
pub mod resolver;
pub mod span;
mod doc;
mod manager;

pub use doc::*;
pub use manager::*;

/// Re-export `lsp_types`
pub use lsp_types;

/// Re-export the syntax crate so downstream users can reach tree-sitter
/// types and the generated typed-node accessors without a separate dep.
pub use greycat_analyzer_syntax as syntax;
