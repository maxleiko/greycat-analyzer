// P6.8 — port of TS `analysis/actions.ts`. P8.3 lands concrete edit
// synthesis (the `changes: Vec<TextEdit>` payload).
//! Code-action vocabulary.
//!
//! Defines the `CodeActionCategory` enum used to tag code actions
//! produced by the analyzer / linter so the LSP layer can group,
//! filter, and prioritize them. This module freezes the category
//! vocabulary so the seam between analysis and LSP doesn't drift.

use std::ops::Range;

/// One edit to be applied as part of a code action.
/// TODO: this should be replaced by the lsp_types proper thing we gain nothing but noise by defining it ourselves
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub new_text: String,
    pub byte_range: Range<usize>,
}
