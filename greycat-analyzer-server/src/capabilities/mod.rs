//! LSP capability handlers.
//!
//! Each capability lives in its own sub-module and produces an LSP
//! response value directly so the same code is callable from the CLI,
//! wasm, and integration tests without going through JSON-RPC.
//!
//! Position handling: LSP positions are 0-indexed `(line, character)`
//! and the rest of this codebase treats `character` as a byte column
//! (matching tree-sitter's `Point.column`). All conversions go through
//! [`crate::conv::position_to_byte`] / [`crate::conv::byte_to_position`]
//! for consistency.

mod code_actions;
mod diagnostics;
mod document_highlights;
mod document_symbols;
mod folding_ranges;
mod formatting;
mod goto;
mod hover;
mod inlay_hints;
mod references_rename;
mod selection_ranges;
mod semantic_tokens;
mod signature_help;
mod workspace_symbols;

pub use code_actions::code_actions_with_project;
pub use diagnostics::diagnostics_from_module;
pub use document_highlights::document_highlights;
pub use document_symbols::document_symbols;
pub use folding_ranges::folding_ranges;
pub use formatting::{formatting, range_formatting};
pub use goto::{
    cross_module_decl_location, cross_module_member_location, cursor_ident_idx,
    goto_declaration_across_project, goto_definition, goto_definition_across_project,
    goto_implementation, goto_implementation_across_project, goto_module_segment,
};
pub use greycat_analyzer_analysis::ide::completion::{
    LibVersionPayload, completion_with_project, extract_lib_version_placeholder,
    resolve_library_version_completion,
};
pub use hover::hover_with_project;
pub use inlay_hints::inlay_hints_with_project;
pub use references_rename::{
    RenameTarget, prepare_rename, references_across_project, rename_across_project,
    resolve_rename_target,
};
pub use selection_ranges::selection_ranges;
pub use semantic_tokens::{SEMANTIC_TOKEN_TYPES, semantic_tokens};
pub use signature_help::signature_help;
pub use workspace_symbols::workspace_symbols;
