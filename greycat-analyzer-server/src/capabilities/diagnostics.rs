//! Thin converter from the IDE-shape `analysis::ide::diagnostics::Diagnostic`
//! ADT to `lsp_types::Diagnostic`. Production logic lives in the analysis
//! crate so the CLI's `lint` command and the wasm bridge share the same
//! source.

use greycat_analyzer_analysis::ide::diagnostics::{
    Diagnostic as IdeDiagnostic, Severity, Tag, from_module,
};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_analysis::project::ModuleAnalysis;
use greycat_analyzer_core::SourceEncoding;
use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString, Position, Range};

pub fn diagnostics_from_module(
    text: &str,
    module: &ModuleAnalysis,
    lint_libs: bool,
    encoding: SourceEncoding,
) -> Vec<Diagnostic> {
    from_module(text, module, lint_libs, encoding)
        .into_iter()
        .map(to_lsp)
        .collect()
}

fn to_lsp(d: IdeDiagnostic) -> Diagnostic {
    Diagnostic {
        range: range_to_lsp(d.range),
        severity: Some(match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Hint => DiagnosticSeverity::HINT,
        }),
        code: Some(NumberOrString::String(d.code)),
        source: Some(d.source),
        message: d.message,
        tags: d.tag.map(|t| {
            vec![match t {
                Tag::Unnecessary => DiagnosticTag::UNNECESSARY,
                Tag::Deprecated => DiagnosticTag::DEPRECATED,
            }]
        }),
        ..Default::default()
    }
}

fn range_to_lsp(r: IdeRange) -> Range {
    Range {
        start: pos_to_lsp(r.start),
        end: pos_to_lsp(r.end),
    }
}

fn pos_to_lsp(p: IdePosition) -> Position {
    Position {
        line: p.line,
        character: p.character,
    }
}
