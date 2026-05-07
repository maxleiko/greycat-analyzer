//! Code-action vocabulary (P6.8 — port of TS `analysis/actions.ts`).
//!
//! Defines the `CodeActionCategory` enum used to tag code actions
//! produced by the analyzer / linter so the LSP layer can group,
//! filter, and prioritize them. Concrete edit synthesis (the
//! `changes: Vec<TextEdit>` payload) lands in P8.3 — this module just
//! freezes the category vocabulary so the seam between analysis and
//! LSP doesn't drift.

use std::ops::Range;

/// Mirror of TS `GreyCatCodeActionCategory`. Stable identifier for an
/// action's *intent*; the LSP `CodeActionKind` (`quickfix`,
/// `refactor`, …) lives separately on the LSP side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeActionCategory {
    RewriteTaskToFn,
    V6UseStmt,
    ExtractAnonymousType,
    V6GeoLiteral,
    V6Constructor,
    TypeError,
    InvalidModVar,
    Security,
    Deprecated,
    ProbablyNotWhatYouMeant,
    Visibility,
    MightThrow,
    TypeInference,
    Project,
    Unused,
}

impl CodeActionCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodeActionCategory::RewriteTaskToFn => "rewrite_task_to_fn",
            CodeActionCategory::V6UseStmt => "v6_use_stmt",
            CodeActionCategory::ExtractAnonymousType => "extract_anonymous_type",
            CodeActionCategory::V6GeoLiteral => "v6_geo_literal",
            CodeActionCategory::V6Constructor => "v6_constructor",
            CodeActionCategory::TypeError => "type_error",
            CodeActionCategory::InvalidModVar => "invalid_modvar",
            CodeActionCategory::Security => "security",
            CodeActionCategory::Deprecated => "deprecated",
            CodeActionCategory::ProbablyNotWhatYouMeant => "probably_not_what_you_meant",
            CodeActionCategory::Visibility => "visibility",
            CodeActionCategory::MightThrow => "might_throw",
            CodeActionCategory::TypeInference => "type_inference",
            CodeActionCategory::Project => "project",
            CodeActionCategory::Unused => "unused",
        }
    }
}

/// One edit to be applied as part of a code action.
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub new_text: String,
    pub byte_range: Range<usize>,
}

/// Analyzer-side description of a code action. Translated to an LSP
/// `CodeAction` at the capability boundary in P8.3.
#[derive(Debug, Clone)]
pub struct CodeAction {
    pub category: CodeActionCategory,
    pub title: String,
    pub changes: Vec<TextEdit>,
}
