//! Persistent `Project` handle exposed to JS via `#[wasm_bindgen]`.
//!
//! Wraps the same `(SourceManager, ProjectAnalysis)` pair that the LSP
//! `Backend` carries per-project, but with no filesystem I/O and no
//! `lsp-server` channels: the file set comes in pre-loaded from JS at
//! construction time, and capability calls translate directly to method
//! calls.
//!
//! The host (JS) is responsible for fetching / unzipping / caching the
//! stdlib + project closure and handing the wasm side the `Map<uri, text>`
//! before any analysis runs. See P41.9 for the JS-side helper.
//!
//! Out of scope here (lands in later chunks):
//! - Multi-project routing (`Backend::projects` keyed by root). One
//!   wasm `Project` per JS-side instance.
//! - Project-entrypoint pragma re-walks on entrypoint edits. The wasm
//!   side currently treats every `change` as a body edit. Pragma edits
//!   that add `@library` / `@include` won't pull in new files until
//!   the JS host re-`new`s a project.

use std::str::FromStr;

use greycat_analyzer_analysis::ide::completion::{CompletionList, completion_with_project};
use greycat_analyzer_analysis::ide::diagnostics::{Diagnostic, from_module};
use greycat_analyzer_analysis::ide::document_highlights::{DocumentHighlight, document_highlights};
use greycat_analyzer_analysis::ide::document_symbols::{DocumentSymbol, document_symbols};
use greycat_analyzer_analysis::ide::folding_ranges::{FoldingRange, folding_ranges};
use greycat_analyzer_analysis::ide::hover::{Hover, hover_with_project};
use greycat_analyzer_analysis::ide::inlay_hints::{InlayHint, inlay_hints_with_project};
use greycat_analyzer_analysis::ide::semantic_tokens::{SemanticTokens, semantic_tokens};
use greycat_analyzer_analysis::ide::signature_help::{SignatureHelp, signature_help};
use greycat_analyzer_analysis::ide::types::{Position as IdePosition, Range as IdeRange, TextEdit};
use greycat_analyzer_analysis::ide::workspace_symbols::{WorkspaceSymbol, workspace_symbols};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::conv::byte_to_position;
use greycat_analyzer_core::lsp_types::{Position, Uri};
use wasm_bindgen::prelude::*;

/// JS-facing handle to a loaded GreyCat project. Owns the
/// `SourceManager` (parsed sources) and `ProjectAnalysis` (cached
/// per-module HIR + signatures + bodies + lints).
#[wasm_bindgen]
pub struct Project {
    manager: SourceManager,
    analysis: ProjectAnalysis,
    encoding: SourceEncoding,
}

#[wasm_bindgen]
impl Project {
    /// Construct a project from a pre-loaded `Map<uri: string, text: string>`.
    /// `entrypoint_uri` is the URI of the project's `project.gcl` —
    /// declared here even though the wasm side doesn't walk pragmas
    /// itself, so future chunks can read it without re-deriving from
    /// the map.
    ///
    /// Each entry's `lib` tag is inferred from its URI path: files
    /// under `.../lib/<name>/...` get `lib = "<name>"`; everything
    /// else gets `lib = "project"`. The JS host produces the right
    /// layout when it fetches + unzips the stdlib + project closure.
    #[wasm_bindgen(constructor)]
    pub fn new(entrypoint_uri: &str, files: &js_sys::Map) -> Result<Project, JsValue> {
        // Validate the entrypoint URI early so the JS caller gets a
        // clean error instead of a silent no-op deep in analysis.
        let _entrypoint: Uri = Uri::from_str(entrypoint_uri)
            .map_err(|_| JsValue::from_str(&format!("invalid entrypoint URI: {entrypoint_uri}")))?;

        let mut manager = SourceManager::new();
        let mut err: Option<JsValue> = None;
        files.for_each(&mut |value, key| {
            if err.is_some() {
                return;
            }
            let (Some(key_s), Some(val_s)) = (key.as_string(), value.as_string()) else {
                err = Some(JsValue::from_str(
                    "files map: every key and value must be a string",
                ));
                return;
            };
            let Ok(uri) = Uri::from_str(&key_s) else {
                err = Some(JsValue::from_str(&format!("invalid URI: {key_s}")));
                return;
            };
            let lib = lib_from_uri(&uri);
            manager.add_simple(uri, val_s, lib, false);
        });
        if let Some(e) = err {
            return Err(e);
        }

        let mut analysis = ProjectAnalysis::new();
        analysis.rebuild(&manager);
        Ok(Self {
            manager,
            analysis,
            encoding: SourceEncoding::UTF16,
        })
    }

    /// Editor opened a file — install (or refresh) its text and mark
    /// it as opened. Triggers per-URI invalidation, so subsequent
    /// `diagnostics(uri)` calls return up-to-date results.
    pub fn open(&mut self, uri: &str, source: String) -> Result<(), JsValue> {
        let uri = parse_uri(uri)?;
        let lib = self.lib_for(&uri);
        self.manager.add_simple(uri.clone(), source, lib, true);
        self.analysis.invalidate(&self.manager, &uri);
        Ok(())
    }

    /// Editor changed a file. Full-text replacement — incremental
    /// `TextDocumentContentChangeEvent` ranges are an LSP-wire detail
    /// the JS host can flatten before calling.
    pub fn change(&mut self, uri: &str, source: String) -> Result<(), JsValue> {
        let uri = parse_uri(uri)?;
        let lib = self.lib_for(&uri);
        self.manager.add_simple(uri.clone(), source, lib, true);
        self.analysis.invalidate(&self.manager, &uri);
        Ok(())
    }

    /// Editor closed a file. The document stays in the manager (it
    /// may still be reachable through the project's pragma closure);
    /// we only drop the `opened` flag so future tooling can distinguish
    /// editor-resident files from background-loaded ones.
    pub fn close(&mut self, uri: &str) -> Result<(), JsValue> {
        let uri = parse_uri(uri)?;
        if let Some(cell) = self.manager.get(&uri) {
            cell.borrow_mut().opened = false;
        }
        Ok(())
    }

    /// Pull-based diagnostics for a single URI. Returns an empty vec
    /// when the URI is unknown (rather than erroring) so the JS host
    /// can treat the call as idempotent.
    pub fn diagnostics(&self, uri: &str) -> Result<Vec<Diagnostic>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        let Some(module) = self.analysis.module(&uri) else {
            return Ok(Vec::new());
        };
        // `lint_libs = false` matches the LSP default — users editing
        // a project don't want lints on the stdlib they don't own.
        Ok(from_module(&doc.text, module, false, self.encoding))
    }

    /// Hover at `(line, character)` in `uri`. Returns `None` when the
    /// URI is unknown or when nothing under the cursor produces hover
    /// content.
    pub fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Option<Hover>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(None);
        };
        let doc = cell.borrow();
        let pos = Position { line, character };
        Ok(hover_with_project(
            &doc.text,
            &doc.lib,
            doc.root_node(),
            pos,
            &uri,
            &self.analysis,
            &self.manager,
            self.encoding,
        ))
    }

    /// Folding regions for the given URI. Empty vec for unknown URIs.
    #[wasm_bindgen(js_name = foldingRanges)]
    pub fn folding_ranges(&self, uri: &str) -> Result<Vec<FoldingRange>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        Ok(folding_ranges(&doc.text, doc.root_node(), self.encoding))
    }

    /// Project-wide symbol search filtered by case-insensitive substring
    /// match against `query`. Walks every loaded document in the
    /// `SourceManager` (project + libraries).
    #[wasm_bindgen(js_name = workspaceSymbols)]
    pub fn workspace_symbols(&self, query: &str) -> Vec<WorkspaceSymbol> {
        let docs: Vec<_> = self
            .manager
            .iter()
            .map(|(uri, cell)| {
                let doc = cell.borrow();
                (uri.clone(), doc.lib.clone(), doc.text.clone())
            })
            .collect();
        workspace_symbols(docs, query, self.encoding)
    }

    /// Outline tree — top-level decls with type members as children.
    #[wasm_bindgen(js_name = documentSymbols)]
    pub fn document_symbols(&self, uri: &str) -> Result<Vec<DocumentSymbol>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        Ok(document_symbols(
            &doc.text,
            &doc.lib,
            doc.root_node(),
            self.encoding,
        ))
    }

    /// Same-spelling identifier occurrences in the given URI.
    #[wasm_bindgen(js_name = documentHighlights)]
    pub fn document_highlights(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<DocumentHighlight>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        let pos = Position { line, character };
        Ok(document_highlights(
            &doc.text,
            doc.root_node(),
            pos,
            self.encoding,
        ))
    }

    /// Inlay hints overlapping the given `(start_line, start_character,
    /// end_line, end_character)` viewport. Empty vec for unknown URIs.
    #[wasm_bindgen(js_name = inlayHints)]
    pub fn inlay_hints(
        &self,
        uri: &str,
        start_line: u32,
        start_character: u32,
        end_line: u32,
        end_character: u32,
    ) -> Result<Vec<InlayHint>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        let Some(module) = self.analysis.module(&uri) else {
            return Ok(Vec::new());
        };
        let range = IdeRange {
            start: IdePosition {
                line: start_line,
                character: start_character,
            },
            end: IdePosition {
                line: end_line,
                character: end_character,
            },
        };
        Ok(inlay_hints_with_project(
            module,
            &self.analysis,
            &doc.text,
            &range,
            self.encoding,
        ))
    }

    /// Delta-encoded semantic tokens for the whole file. Returns an
    /// empty `SemanticTokens` for unknown URIs.
    #[wasm_bindgen(js_name = semanticTokens)]
    pub fn semantic_tokens(&self, uri: &str) -> Result<SemanticTokens, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(SemanticTokens::default());
        };
        let doc = cell.borrow();
        Ok(semantic_tokens(
            &doc.text,
            &doc.lib,
            doc.root_node(),
            self.encoding,
        ))
    }

    /// Scope / member / static / type-position / object-field /
    /// directive / pragma completion at the cursor. Returns `None` when
    /// the URI is unknown or when no completion source produces a list.
    pub fn completion(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<CompletionList>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(None);
        };
        let doc = cell.borrow();
        let pos = Position { line, character };
        // `project_root: None` — wasm has no filesystem, so the
        // `@include` directory walk in [`include_dir_completion`] short-
        // circuits. Everything else (scope, member, static, type-position,
        // object-field, directive, pragma, library-version) works the
        // same as the LSP path.
        Ok(completion_with_project(
            &doc.text,
            doc.root_node(),
            pos,
            &uri,
            &self.analysis,
            None,
            self.encoding,
        ))
    }

    /// Signature help when the cursor is inside a `call_expr`.
    #[wasm_bindgen(js_name = signatureHelp)]
    pub fn signature_help(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<SignatureHelp>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(None);
        };
        let doc = cell.borrow();
        let pos = Position { line, character };
        Ok(signature_help(
            &doc.text,
            &doc.lib,
            doc.root_node(),
            pos,
            self.encoding,
        ))
    }

    /// Whole-document formatting. Returns a single full-range edit
    /// when the formatter's output differs; an empty vec otherwise.
    /// Empty vec is also returned for unknown URIs (idempotent shape).
    pub fn format(&self, uri: &str) -> Result<Vec<TextEdit>, JsValue> {
        let uri = parse_uri(uri)?;
        let Some(cell) = self.manager.get(&uri) else {
            return Ok(Vec::new());
        };
        let doc = cell.borrow();
        let formatted = greycat_analyzer_fmt::format_tree(&doc.text, doc.root_node());
        if formatted == doc.text {
            return Ok(Vec::new());
        }
        let end = byte_to_position(&doc.text, doc.text.len(), self.encoding);
        Ok(vec![TextEdit {
            range: IdeRange {
                start: IdePosition {
                    line: 0,
                    character: 0,
                },
                end: IdePosition {
                    line: end.line,
                    character: end.character,
                },
            },
            new_text: formatted,
        }])
    }
}

fn parse_uri(s: &str) -> Result<Uri, JsValue> {
    Uri::from_str(s).map_err(|_| JsValue::from_str(&format!("invalid URI: {s}")))
}

impl Project {
    fn lib_for(&self, uri: &Uri) -> String {
        if let Some(cell) = self.manager.get(uri) {
            return cell.borrow().lib.clone();
        }
        lib_from_uri(uri)
    }
}

/// Derive the `lib` tag from a URI path. Anything under `.../lib/<name>/...`
/// belongs to library `<name>`; everything else is project source.
fn lib_from_uri(uri: &Uri) -> String {
    let s = uri.as_str();
    if let Some(idx) = s.rfind("/lib/") {
        let rest = &s[idx + "/lib/".len()..];
        if let Some(slash) = rest.find('/') {
            return rest[..slash].to_string();
        }
    }
    "project".into()
}
