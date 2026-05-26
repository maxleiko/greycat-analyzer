//! Code actions / quickfix synthesis as IDE-shape ADTs. The LSP
//! server's `capabilities/code_actions.rs` converts to
//! `lsp_types::CodeAction` + `WorkspaceEdit` at the wire boundary; the
//! wasm bridge consumes the flat `Vec<UriEdits>` shape directly.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{SourceEncoding, SymbolTable};
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_syntax::tree_sitter;

use crate::conv::{position_to_byte, ranges_overlap};
use crate::ide::diagnostics::{Diagnostic, from_module};
use crate::ide::quickfix::{QuickfixCx, edit_for_diagnostic};
use crate::ide::types::{Range, TextEdit};
use crate::project::ModuleAnalysis;

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeActionKind {
    QuickFix,
}

/// Edits grouped by URI — the flat replacement for LSP's
/// `WorkspaceEdit { changes: HashMap<Uri, Vec<TextEdit>> }`. Almost
/// every diagnostic-driven action touches a single URI, but the
/// multi-URI shape is preserved so future fixes (e.g. rename + import
/// rewrite) compose cleanly.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct UriEdits {
    #[cfg_attr(feature = "wasm", wasm_bindgen(skip))]
    pub uri: Uri,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub edits: Vec<TextEdit>,
}

#[cfg(feature = "wasm")]
#[wasm_bindgen]
impl UriEdits {
    #[wasm_bindgen(getter)]
    pub fn uri(&self) -> String {
        self.uri.as_str().to_string()
    }
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct CodeAction {
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub title: String,
    pub kind: CodeActionKind,
    /// The diagnostic this action fixes — embedded inline so the LSP
    /// converter can lift it back into the `CodeAction.diagnostics`
    /// slot without re-deriving from byte ranges.
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub diagnostic: Diagnostic,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub edits: Vec<UriEdits>,
}

#[allow(clippy::too_many_arguments)]
pub fn code_actions_with_project(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    text: &str,
    root: tree_sitter::Node<'_>,
    uri: &Uri,
    range: Range,
    encoding: SourceEncoding,
) -> Vec<CodeAction> {
    // Code actions don't differentiate lib vs project — the user's
    // already pointing at a specific diagnostic when invoking them.
    let semantic = from_module(text, module, true, encoding);
    code_actions_from_diagnostics(module, symbols, root, text, uri, range, semantic, encoding)
}

#[allow(clippy::too_many_arguments)]
fn code_actions_from_diagnostics(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    root: tree_sitter::Node<'_>,
    text: &str,
    uri: &Uri,
    range: Range,
    semantic: Vec<Diagnostic>,
    encoding: SourceEncoding,
) -> Vec<CodeAction> {
    semantic
        .into_iter()
        .filter(|d| ranges_overlap_ide(&d.range, &range))
        .map(|d| {
            let raw_edits = synthesize_fix(module, symbols, root, text, &d, encoding);
            // **P22.5** — never offer an edit whose application would
            // break the document's parse. Apply the edit in-memory,
            // re-parse, and drop the edit if it adds new parse errors
            // the original didn't have. Mirrors the cli `--fix` safety
            // net.
            let edits = if !raw_edits.is_empty() && would_break_parse(text, &raw_edits, encoding) {
                Vec::new()
            } else {
                raw_edits
            };
            let title = if edits.is_empty() {
                format!("Quickfix: {}", d.message)
            } else {
                format!("Fix: {}", d.message)
            };
            CodeAction {
                title,
                kind: CodeActionKind::QuickFix,
                diagnostic: d,
                edits: if edits.is_empty() {
                    Vec::new()
                } else {
                    vec![UriEdits {
                        uri: uri.clone(),
                        edits,
                    }]
                },
            }
        })
        .collect()
}

fn ranges_overlap_ide(a: &Range, b: &Range) -> bool {
    let lsp_a = greycat_analyzer_core::lsp_types::Range {
        start: greycat_analyzer_core::lsp_types::Position {
            line: a.start.line,
            character: a.start.character,
        },
        end: greycat_analyzer_core::lsp_types::Position {
            line: a.end.line,
            character: a.end.character,
        },
    };
    let lsp_b = greycat_analyzer_core::lsp_types::Range {
        start: greycat_analyzer_core::lsp_types::Position {
            line: b.start.line,
            character: b.start.character,
        },
        end: greycat_analyzer_core::lsp_types::Position {
            line: b.end.line,
            character: b.end.character,
        },
    };
    ranges_overlap(&lsp_a, &lsp_b)
}

/// Apply `edits` against `text` in-memory and check if the result has
/// new parse errors. Returns `true` if the edit would break a
/// previously-valid parse. Used to gate quickfix offers.
fn would_break_parse(text: &str, edits: &[TextEdit], encoding: SourceEncoding) -> bool {
    let original_has_errors = greycat_analyzer_syntax::parse(text).root_node().has_error();
    let mut byte_edits: Vec<(std::ops::Range<usize>, &str)> = edits
        .iter()
        .map(|e| {
            let lsp_start = greycat_analyzer_core::lsp_types::Position {
                line: e.range.start.line,
                character: e.range.start.character,
            };
            let lsp_end = greycat_analyzer_core::lsp_types::Position {
                line: e.range.end.line,
                character: e.range.end.character,
            };
            (
                position_to_byte(text, lsp_start, encoding)
                    ..position_to_byte(text, lsp_end, encoding),
                e.new_text.as_str(),
            )
        })
        .collect();
    byte_edits.sort_by_key(|(r, _)| r.start);
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

fn synthesize_fix(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    root: tree_sitter::Node<'_>,
    text: &str,
    diag: &Diagnostic,
    encoding: SourceEncoding,
) -> Vec<TextEdit> {
    let code = diag.code.as_str();
    let lsp_start = greycat_analyzer_core::lsp_types::Position {
        line: diag.range.start.line,
        character: diag.range.start.character,
    };
    let lsp_end = greycat_analyzer_core::lsp_types::Position {
        line: diag.range.end.line,
        character: diag.range.end.character,
    };
    let start = position_to_byte(text, lsp_start, encoding);
    let end = position_to_byte(text, lsp_end, encoding);
    let cx = QuickfixCx {
        root,
        text,
        hir: Some(&module.hir),
        symbols: Some(symbols),
    };
    let _ = Decl::Fn; // silence unused import warning when feature graph excludes this branch
    edit_for_diagnostic(&cx, code, &(start..end), &diag.message)
        .into_iter()
        .map(|e| TextEdit {
            range: Range::from_byte_range(text, &e.byte_range, encoding),
            new_text: e.new_text,
        })
        .collect()
}
