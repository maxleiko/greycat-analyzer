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
use greycat_analyzer_core::diagnostics::parse_diagnostics as core_parse_diagnostics;
use greycat_analyzer_core::lsp_types;
use greycat_analyzer_core::lsp_types::{DiagnosticSeverity as LspSeverity, NumberOrString};
use greycat_analyzer_syntax::tree_sitter;

use crate::analyzer::Severity as AnalyzerSeverity;
use crate::ide::types::{Position, Range};
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

/// Parse-stage diagnostics projected into the IDE-shape `Diagnostic`.
/// Wraps `greycat_analyzer_core::diagnostics::parse_diagnostics` —
/// the LSP server calls the core function directly so it can run on
/// every keystroke without waiting for the analyzer, but the wasm
/// bridge wants one combined IDE-shape vec per pull.
pub fn parse_from_tree(
    root: tree_sitter::Node<'_>,
    text: &str,
    _encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    // The core helper already produces `lsp_types::Diagnostic` with
    // resolved `Range` positions under the negotiated encoding, so we
    // just project shape-to-shape here.
    core_parse_diagnostics(root, text, _encoding)
        .into_iter()
        .map(lsp_to_ide)
        .collect()
}

/// Combined parse + semantic + lint diagnostics for a single module.
/// The wasm `Project::diagnostics` entry point — every pull from the
/// editor returns one merged vec.
pub fn from_document(
    text: &str,
    root: tree_sitter::Node<'_>,
    module: &ModuleAnalysis,
    lint_libs: bool,
    encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    let mut out = parse_from_tree(root, text, encoding);
    out.extend(from_module(text, module, lint_libs, encoding));
    out
}

fn lsp_to_ide(d: lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line: d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: lsp_severity_from(d.severity),
        code: match d.code {
            Some(NumberOrString::String(s)) => s,
            Some(NumberOrString::Number(n)) => n.to_string(),
            None => String::new(),
        },
        source: d.source.unwrap_or_else(|| "greycat-analyzer".into()),
        message: d.message,
        tag: None,
    }
}

fn lsp_severity_from(s: Option<LspSeverity>) -> Severity {
    match s {
        Some(LspSeverity::WARNING) => Severity::Warning,
        Some(LspSeverity::INFORMATION) | Some(LspSeverity::HINT) => Severity::Hint,
        // `parse_diagnostics` always emits `Some(ERROR)` — the `None`
        // fallback only matters if a future caller forgets to set it,
        // which we still want surfaced rather than silently dropped.
        _ => Severity::Error,
    }
}
