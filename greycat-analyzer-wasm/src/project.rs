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

use greycat_analyzer_analysis::ide::diagnostics::{Diagnostic, from_module};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
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
