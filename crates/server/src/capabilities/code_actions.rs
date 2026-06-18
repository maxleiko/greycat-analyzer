//! Thin converter from the IDE-shape `analysis::ide::code_actions::*`
//! ADTs to `lsp_types::CodeActionOrCommand`. The dispatcher, parse-
//! safety gate, and quickfix synthesis all live in the analysis crate
//! so the wasm bridge consumes the flat `Vec<UriEdits>` shape directly.

use greycat_analyzer_analysis::ide::code_actions::{
    CodeAction as IdeCodeAction, CodeActionKind as IdeCodeActionKind, UriEdits,
    code_actions_with_project as code_actions_inner,
};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange};
use greycat_analyzer_analysis::project::ModuleAnalysis;
use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_syntax::tree_sitter;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
};

use super::diagnostics::diagnostics_from_module;

#[allow(clippy::too_many_arguments)]
pub fn code_actions_with_project(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    text: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    range: Range,
    encoding: SourceEncoding,
) -> Vec<CodeActionOrCommand> {
    let ide_range = IdeRange {
        start: IdePosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: IdePosition {
            line: range.end.line,
            character: range.end.character,
        },
    };
    let actions = code_actions_inner(module, symbols, text, root, uri, ide_range, encoding);
    let lsp_diagnostics = diagnostics_from_module(text, module, true, encoding);
    actions
        .into_iter()
        .map(|a| to_lsp(a, &lsp_diagnostics))
        .collect()
}

fn to_lsp(action: IdeCodeAction, lsp_diagnostics: &[lsp_types::Diagnostic]) -> CodeActionOrCommand {
    // The analysis-side carries its own IDE `Diagnostic` shape on each
    // action. We re-pair with the equivalent `lsp_types::Diagnostic`
    // from the same module pass so the `CodeAction.diagnostics` slot
    // carries the wire-shape diagnostic the client emitted.
    let lsp_diag = lsp_diagnostics
        .iter()
        .find(|d| {
            d.range.start.line == action.diagnostic.range.start.line
                && d.range.start.character == action.diagnostic.range.start.character
                && d.range.end.line == action.diagnostic.range.end.line
                && d.range.end.character == action.diagnostic.range.end.character
                && d.message == action.diagnostic.message
        })
        .cloned();
    let kind = match action.kind {
        IdeCodeActionKind::QuickFix => CodeActionKind::QUICKFIX,
    };
    let edit = if action.edits.is_empty() {
        // Empty-edit "Quickfix:" entries — keep the action so the user
        // can still see the diagnostic in the code-action list, but
        // skip the WorkspaceEdit field.
        None
    } else {
        Some(workspace_edit_from(action.edits))
    };
    CodeActionOrCommand::CodeAction(CodeAction {
        title: action.title,
        kind: Some(kind),
        diagnostics: lsp_diag.map(|d| vec![d]),
        edit,
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

fn workspace_edit_from(uri_edits: Vec<UriEdits>) -> WorkspaceEdit {
    #[allow(clippy::mutable_key_type)]
    let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
        std::collections::HashMap::new();
    for ue in uri_edits {
        let lsp_edits: Vec<TextEdit> = ue
            .edits
            .into_iter()
            .map(|e| TextEdit {
                range: range_to_lsp(e.range),
                new_text: e.new_text,
            })
            .collect();
        changes.entry(ue.uri).or_default().extend(lsp_edits);
    }
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
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
