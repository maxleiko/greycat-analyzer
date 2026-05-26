//! Code actions / quickfix synthesis. Reads the cached `ModuleAnalysis`
//! diagnostics from `ProjectAnalysis` and maps each fixable finding to
//! a `TextEdit`. The parse-safety gate re-parses with the edit applied
//! and discards anything that would introduce new parse errors.

use greycat_analyzer_analysis::{ide, project::ModuleAnalysis};
use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Diagnostic, NumberOrString, TextEdit, Uri,
    WorkspaceEdit,
};

use super::diagnostics::diagnostics_from_module;
use crate::conv::{byte_to_position, position_to_byte, ranges_overlap};

/// Project-aware variant — reads the cached diagnostics + lints from
/// the [`greycat_analyzer_analysis::project::ProjectAnalysis`] entry for
/// `uri` instead of re-running the whole pipeline. Same convention as
/// the rest of the `*_with_project` family: the LSP server handler in
/// [`crate::server`] always goes through this path so the cross-module
/// fixup passes feed into the diagnostic list.
pub fn code_actions_with_project(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    text: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    range: lsp_types::Range,
    encoding: SourceEncoding,
) -> Vec<CodeActionOrCommand> {
    // Code actions don't differentiate lib vs project — the user's
    // already pointing at a specific diagnostic when invoking them.
    let semantic = diagnostics_from_module(text, module, true, encoding);
    code_actions_from_diagnostics(module, symbols, root, text, uri, range, semantic, encoding)
}

#[allow(clippy::too_many_arguments)]
fn code_actions_from_diagnostics(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    root: tree_sitter::Node<'_>,
    text: &str,
    uri: &Uri,
    range: lsp_types::Range,
    semantic: Vec<Diagnostic>,
    encoding: SourceEncoding,
) -> Vec<CodeActionOrCommand> {
    semantic
        .into_iter()
        .filter(|d| ranges_overlap(&d.range, &range))
        .map(|d| {
            let raw_edits = synthesize_fix(module, symbols, root, text, &d, encoding);
            // **P22.5** — never offer an edit whose application would
            // break the document's parse. Apply the edit in-memory,
            // re-parse, and drop the edit if it adds new parse errors
            // the original didn't have. Mirrors the cli `--fix`
            // safety net.
            let edits = if !raw_edits.is_empty() && would_break_parse(text, &raw_edits, encoding) {
                Vec::new()
            } else {
                raw_edits
            };
            let title = match edits.is_empty() {
                true => format!("Quickfix: {}", d.message),
                false => format!("Fix: {}", d.message),
            };
            CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![d.clone()]),
                edit: Some(WorkspaceEdit {
                    changes: Some({
                        #[allow(clippy::mutable_key_type)]
                        let mut m = std::collections::HashMap::new();
                        m.insert(uri.clone(), edits);
                        m
                    }),
                    document_changes: None,
                    change_annotations: None,
                }),
                command: None,
                is_preferred: None,
                disabled: None,
                data: None,
            })
        })
        .collect()
}

/// Apply `edits` against `text` in-memory and check if the result has
/// new parse errors. Returns `true` if the edit would break a
/// previously-valid parse. Used to gate quickfix offers.
fn would_break_parse(text: &str, edits: &[TextEdit], encoding: SourceEncoding) -> bool {
    let original_has_errors = greycat_analyzer_syntax::parse(text).root_node().has_error();
    // Apply edits in reverse byte order so prior offsets stay stable.
    let mut byte_edits: Vec<(std::ops::Range<usize>, &str)> = edits
        .iter()
        .map(|e| {
            (
                position_to_byte(text, e.range.start, encoding)
                    ..position_to_byte(text, e.range.end, encoding),
                e.new_text.as_str(),
            )
        })
        .collect();
    byte_edits.sort_by_key(|(r, _)| r.start);
    // Drop overlapping edits.
    let mut last_end = 0usize;
    let mut clean: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    for (r, t) in byte_edits {
        if r.start < last_end {
            continue;
        }
        last_end = r.end;
        clean.push((r, t));
    }
    let mut new_text = text.to_string();
    for (r, t) in clean.into_iter().rev() {
        new_text.replace_range(r, t);
    }
    let new_has_errors = greycat_analyzer_syntax::parse(&new_text)
        .root_node()
        .has_error();
    new_has_errors && !original_has_errors
}

/// Map a diagnostic to a concrete `Vec<TextEdit>`. Routes through the
/// shared [`greycat_analyzer_analysis::ide::quickfix`] module so the LSP and
/// the cli `lint --fix` path share a single source of truth.
fn synthesize_fix(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    root: tree_sitter::Node<'_>,
    text: &str,
    diag: &Diagnostic,
    encoding: SourceEncoding,
) -> Vec<TextEdit> {
    let code = match &diag.code {
        Some(NumberOrString::String(s)) => s.as_str(),
        _ => return Vec::new(),
    };
    let start = position_to_byte(text, diag.range.start, encoding);
    let end = position_to_byte(text, diag.range.end, encoding);
    let cx = ide::quickfix::QuickfixCx {
        root,
        text,
        hir: Some(&module.hir),
        symbols: Some(symbols),
    };
    let edits = ide::quickfix::edit_for_diagnostic(&cx, code, &(start..end), &diag.message);
    edits
        .into_iter()
        .map(|e| TextEdit {
            range: lsp_types::Range {
                start: byte_to_position(text, e.byte_range.start, encoding),
                end: byte_to_position(text, e.byte_range.end, encoding),
            },
            new_text: e.new_text,
        })
        .collect()
}
