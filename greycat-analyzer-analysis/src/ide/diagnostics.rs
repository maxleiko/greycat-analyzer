//! IDE-shape diagnostic ADT — decoupled from `lsp_types`. Constructed
//! at the boundary where source text + encoding are known, so editor
//! and wasm consumers receive resolved Position/Range fields directly
//! instead of byte ranges they'd have to resolve themselves.
//!
//! The internal analysis types (`SemanticDiagnostic`, `LintDiagnostic`)
//! stay byte-range-shaped; this module is the IDE-facing projection.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::SourceEncoding;

use crate::analyzer::Severity as AnalyzerSeverity;
use crate::ide::types::Range;
use crate::lint::{DiagTag, LintSeverity};
use crate::project::ModuleAnalysis;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Unnecessary,
    Deprecated,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub code: String,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub source: String,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub message: String,
    pub tag: Option<Tag>,
}

/// Project-aware diagnostics — read the cached analyzer + lints from
/// `module` and project each finding into an IDE-shape `Diagnostic`.
/// Mirrors the body of the cli `lint` command's per-module conversion;
/// the LSP capability handler converts each entry to
/// `lsp_types::Diagnostic` at the wire boundary.
///
/// `lint_libs` opts into emitting diagnostics for non-project modules
/// (under `lib/<name>/`). Default for editors is `false`: users don't
/// own stdlib / vendored code.
pub fn from_module(
    text: &str,
    module: &ModuleAnalysis,
    lint_libs: bool,
    encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    if !lint_libs && module.lib != "project" {
        return Vec::new();
    }
    let mut out: Vec<Diagnostic> = module
        .analysis
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: Range::from_byte_range(text, &d.byte_range, encoding),
            severity: severity_from(d.severity),
            code: d.code.into(),
            source: "greycat-analyzer".into(),
            message: d.message.clone(),
            tag: None,
        })
        .collect();
    for lint in &module.lints {
        out.push(Diagnostic {
            range: Range::from_byte_range(text, &lint.byte_range, encoding),
            severity: lint_severity_from(lint.severity),
            code: lint.rule.into(),
            source: "lint".into(),
            message: lint.message.clone(),
            tag: lint.tag.map(tag_from),
        });
    }
    out
}

fn severity_from(s: AnalyzerSeverity) -> Severity {
    match s {
        AnalyzerSeverity::Error => Severity::Error,
        AnalyzerSeverity::Warning => Severity::Warning,
        AnalyzerSeverity::Hint => Severity::Hint,
    }
}

fn lint_severity_from(s: LintSeverity) -> Severity {
    match s {
        LintSeverity::Error => Severity::Error,
        LintSeverity::Warning => Severity::Warning,
        LintSeverity::Hint => Severity::Hint,
    }
}

fn tag_from(t: DiagTag) -> Tag {
    match t {
        DiagTag::Unnecessary => Tag::Unnecessary,
        DiagTag::Deprecated => Tag::Deprecated,
    }
}
