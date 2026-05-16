//! Diagnostics conversion — turn cached [`ModuleAnalysis`] semantic
//! diagnostics + lints into LSP `Diagnostic` values. The CLI's `lint`
//! command mirrors this body so editor and terminal output stay in
//! lockstep.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::lint::{DiagTag, LintSeverity, run_lints};
use greycat_analyzer_analysis::project::ModuleAnalysis;
use greycat_analyzer_analysis::resolver::resolve;
use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, NumberOrString};

use crate::conv::byte_range_to_lsp;

// P24.5
/// Translate the analysis crate's [`DiagTag`] into LSP
/// `DiagnosticTag` values. Editors that honor `UNNECESSARY` dim the
/// span ("this code does nothing") and editors that honor `DEPRECATED`
/// strike it through. Returns `None` so the diagnostic's `tags` field
/// stays absent for un-tagged rules (no extra serialized payload).
fn lint_tags(tag: Option<DiagTag>) -> Option<Vec<DiagnosticTag>> {
    let lsp_tag = match tag? {
        DiagTag::Unnecessary => DiagnosticTag::UNNECESSARY,
        DiagTag::Deprecated => DiagnosticTag::DEPRECATED,
    };
    Some(vec![lsp_tag])
}

/// Project-aware diagnostics — read the cached analyzer + lints from
/// the [`ModuleAnalysis`] entry for this module and convert each
/// finding to an `lsp_types::Diagnostic`. Mirrors the body of the cli
/// `lint` command's per-module conversion so the LSP and the
/// CLI surface the same diagnostic shape.
///
/// `lint_libs` opts into emitting diagnostics — both lint and
/// semantic — for non-project modules (anything under `lib/<name>/`).
/// Default for the LSP is `false`: users don't own stdlib / vendored
/// code and don't want noise about it in their editor, including
/// type-relation diagnostics (which can fire from cross-module
/// inference quirks against trusted library code). The VS Code
/// extension exposes this via the `greycat-analyzer.lintLibs`
/// setting; the CLI uses `--lint-libs`.
pub fn diagnostics_from_module(
    text: &str,
    module: &ModuleAnalysis,
    lint_libs: bool,
) -> Vec<Diagnostic> {
    if !lint_libs && module.lib != "project" {
        return Vec::new();
    }
    let mut out: Vec<Diagnostic> = module
        .analysis
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: byte_range_to_lsp(text, &d.byte_range),
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String("semantic".into())),
            source: Some("greycat-analyzer".into()),
            message: d.message.clone(),
            ..Default::default()
        })
        .collect();
    for lint in &module.lints {
        out.push(Diagnostic {
            range: byte_range_to_lsp(text, &lint.byte_range),
            severity: Some(match lint.severity {
                LintSeverity::Error => DiagnosticSeverity::ERROR,
                LintSeverity::Warning => DiagnosticSeverity::WARNING,
                LintSeverity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String(lint.rule.into())),
            source: Some("lint".into()),
            message: lint.message.clone(),
            tags: lint_tags(lint.tag),
            ..Default::default()
        });
    }
    out
}

/// Single-file pipeline (HIR lower → resolver → analyzer + lints) against
/// `text`, returning every finding as `lsp_types::Diagnostic`. Used by
/// the legacy `code_actions` shim — the LSP server's
/// `code_actions_handler` reads from the project cache via
/// `code_actions_with_project` / [`diagnostics_from_module`] instead.
pub(crate) fn current_diagnostics(
    text: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
) -> Vec<Diagnostic> {
    let symbols = SymbolTable::new();
    let hir = lower_module(text, &symbols, "module", lib, root);
    let resolutions = resolve(&hir, &symbols);
    let (_arena, _decl_registry, analysis) =
        greycat_analyzer_analysis::analyzer::analyze(&hir, &resolutions, &symbols);
    let mut out: Vec<Diagnostic> = analysis
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: byte_range_to_lsp(text, &d.byte_range),
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String("semantic".into())),
            source: Some("greycat-analyzer".into()),
            message: d.message.clone(),
            ..Default::default()
        })
        .collect();
    for lint in run_lints(&hir, &resolutions, &symbols) {
        out.push(Diagnostic {
            range: byte_range_to_lsp(text, &lint.byte_range),
            severity: Some(match lint.severity {
                LintSeverity::Error => DiagnosticSeverity::ERROR,
                LintSeverity::Warning => DiagnosticSeverity::WARNING,
                LintSeverity::Hint => DiagnosticSeverity::HINT,
            }),
            code: Some(NumberOrString::String(lint.rule.into())),
            source: Some("lint".into()),
            message: lint.message,
            tags: lint_tags(lint.tag),
            ..Default::default()
        });
    }
    out
}
