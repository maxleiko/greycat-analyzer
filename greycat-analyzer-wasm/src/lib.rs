//! WASM API surface for the greycat analyzer.
//!
//! Two parallel surfaces:
//!
//! 1. **Persistent [`Project`] handle** (always available) — the
//!    `@greycat/analyzer` npm consumer's API. Open / change / close
//!    files, query diagnostics / hover / completion / inlay hints /
//!    references / etc. against a cached `ProjectAnalysis`. Each call
//!    is incremental against the cached state instead of re-running the
//!    pipeline from scratch.
//!
//! 2. **Debug dumpers** (gated behind the `playground` cargo feature)
//!    — `parse_sexp`, `parse_tree`, `tokens`, `lower_hir`, `infer_types`,
//!    `diagnostics`, `format`. Each renders one analyzer stage as JSON
//!    so the playground UI can browse it. These add ~50 KB of JSON-
//!    serialization code, so `@greycat/analyzer` ships without them
//!    (default build). The playground build script enables them via
//!    `wasm-pack build --features playground`.

mod project;

// Re-surface the IDE-shape ADTs from the analysis crate so wasm-bindgen
// emits JS bindings for them at this crate's link boundary.
pub use greycat_analyzer_analysis::ide::code_actions::{CodeAction, CodeActionKind, UriEdits};
pub use greycat_analyzer_analysis::ide::completion::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
    InsertTextFormat,
};
pub use greycat_analyzer_analysis::ide::diagnostics::{
    Diagnostic as IdeDiagnostic, Severity as IdeSeverity, Tag as IdeTag,
};
pub use greycat_analyzer_analysis::ide::document_highlights::DocumentHighlight;
pub use greycat_analyzer_analysis::ide::document_symbols::{DocumentSymbol, SymbolKind};
pub use greycat_analyzer_analysis::ide::folding_ranges::FoldingRange;
pub use greycat_analyzer_analysis::ide::hover::Hover;
pub use greycat_analyzer_analysis::ide::inlay_hints::{InlayHint, InlayHintKind};
pub use greycat_analyzer_analysis::ide::semantic_tokens::SemanticTokens;
pub use greycat_analyzer_analysis::ide::signature_help::{
    ParameterInformation, SignatureHelp, SignatureInformation,
};
pub use greycat_analyzer_analysis::ide::types::{
    Location, Position as IdePosition, Range as IdeRange, TextEdit,
};
pub use greycat_analyzer_analysis::ide::workspace_symbols::WorkspaceSymbol;
pub use project::{Project, RenameTarget};

#[cfg(feature = "playground")]
mod playground;
