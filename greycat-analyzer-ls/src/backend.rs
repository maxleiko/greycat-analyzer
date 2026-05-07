use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use greycat_analyzer_analysis::analyzer::{Severity, analyze};
use greycat_analyzer_analysis::resolver::resolve;
use greycat_analyzer_core::{Document, SourceManager, diagnostics::parse_diagnostics};
use greycat_analyzer_hir::lower_module;
use log::{debug, warn};
use lsp_types::{NumberOrString, Position, Range};
use lsp_server::*;
use lsp_types::{
    notification::{Notification as _, PublishDiagnostics},
    *,
};

use crate::Result;

pub struct Backend {
    pub client: Sender<Message>,
    pub manager: SourceManager,
}

impl Backend {
    fn publish_diagnostics(
        &self,
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) -> Result<()> {
        publish_diagnostics(&self.client, uri, diagnostics, version)
    }

    /// Re-run parse + semantic diagnostics for `uri` and push them to
    /// the client. Idempotent — publishing the same set is fine, the
    /// editor diffs.
    fn publish_for(&self, uri: &Uri) -> Result<()> {
        let Some(cell) = self.manager.get(uri) else {
            return Ok(());
        };
        let doc = cell.borrow();
        let mut diags = parse_diagnostics(doc.root_node(), &doc.text);
        diags.extend(semantic_diagnostics(&doc.text, &doc.lib, doc.root_node()));
        self.publish_diagnostics(uri.clone(), diags, Some(doc.version))
    }

    pub fn initialized(&mut self, init: &InitializeParams) -> Result<()> {
        if let Some(workspaces) = init.workspace_folders.as_ref() {
            debug!("workspaces:");
            for ws in workspaces {
                debug!("- {}={}", ws.name, ws.uri.as_str());
                self.load_workspace(&ws.uri);
            }
        }
        Ok(())
    }

    /// Resolve a workspace-folder URI to a local path, look for
    /// `project.gcl` at its root, and recursively load every reachable
    /// module via [`SourceManager::load_project`]. Errors are logged but
    /// don't fail the LSP handshake — typed diagnostic publication lands
    /// in P1.4.
    fn load_workspace(&mut self, ws_uri: &Uri) {
        let Some(ws_root) = uri_to_path(ws_uri) else {
            warn!("skipping non-file workspace folder: {}", ws_uri.as_str());
            return;
        };
        let project_file = ws_root.join("project.gcl");
        if !project_file.is_file() {
            debug!(
                "no project.gcl in workspace {} — skipping recursive load",
                ws_root.display()
            );
            return;
        }
        let report = self.manager.load_project(&project_file);
        debug!(
            "[load_project] {} files loaded from {}",
            report.loaded.len(),
            project_file.display()
        );
        for lib in &report.unresolved_libraries {
            warn!("unresolved @library('{lib}') in {}", project_file.display());
        }
        for err in &report.errors {
            warn!("[load_project] {err}");
        }
        // Publish parse-diagnostics for every newly-loaded file so the
        // editor lights up red squiggles for syntax errors across the
        // whole project, not just files the user opens.
        for uri in report.loaded {
            if let Err(e) = self.publish_for(&uri) {
                warn!("publish_for({}) failed: {e}", uri.as_str());
            }
        }
    }

    pub fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let doc = Document::new(params.text_document);
        debug!("[did_open] {doc}");
        self.manager.add(doc);
        self.publish_for(&uri)?;
        Ok(())
    }

    pub fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let doc = self.manager.update(
            &uri,
            params.content_changes,
            params.text_document.version,
        );
        debug!("[did_change] {doc}");
        drop(doc);
        self.publish_for(&uri)?;
        Ok(())
    }

    pub fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Result<()> {
        debug!(
            "[did_save] {} (text={:?})",
            params.text_document.uri.as_str(),
            params.text
        );
        // Editors may format/lint on save and send the canonical text;
        // re-publish so any newly-introduced or newly-resolved errors
        // are visible.
        self.publish_for(&params.text_document.uri)?;
        Ok(())
    }

    pub fn did_close(&mut self, params: DidCloseTextDocumentParams) -> Result<()> {
        debug!("[did_close] {}", params.text_document.uri.as_str());
        // Clear diagnostics on close so the editor's stale list goes away.
        self.publish_diagnostics(params.text_document.uri, Vec::new(), None)?;
        Ok(())
    }
}

/// Convert a `file://` URI to a local path. Returns `None` for non-file
/// schemes (LSP technically allows `untitled:`, `git:`, etc.).
fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(Path::new(stripped).to_path_buf())
}

/// Run the full pipeline (HIR lower → resolver → analyzer) and convert
/// every `SemanticDiagnostic` into an `lsp_types::Diagnostic`. The
/// document text and pre-parsed tree are reused so we don't re-parse.
fn semantic_diagnostics(
    text: &str,
    lib: &str,
    root: greycat_analyzer_syntax::tree_sitter::Node<'_>,
) -> Vec<Diagnostic> {
    let hir = lower_module(text, "module", lib, root);
    let resolutions = resolve(&hir);
    let analysis = analyze(&hir, &resolutions);
    analysis
        .diagnostics
        .iter()
        .map(|d| Diagnostic {
            range: byte_range_to_lsp_range(text, &d.byte_range),
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
        .collect()
}

/// Convert a byte range against `text` to an LSP `Range`. The mapping
/// uses byte columns for now (consistent with the rest of the codebase).
fn byte_range_to_lsp_range(text: &str, range: &std::ops::Range<usize>) -> Range {
    fn position_at(text: &str, byte: usize) -> Position {
        let mut line = 0u32;
        let mut col = 0u32;
        let prefix = &text[..byte.min(text.len())];
        for c in prefix.chars() {
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += c.len_utf8() as u32;
            }
        }
        Position {
            line,
            character: col,
        }
    }
    Range {
        start: position_at(text, range.start),
        end: position_at(text, range.end),
    }
}

pub(crate) fn publish_diagnostics(
    client: &Sender<Message>,
    uri: Uri,
    diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
) -> Result<()> {
    debug!("{} diagnotics for {}", diagnostics.len(), uri.as_str());
    let params = PublishDiagnosticsParams::new(uri, diagnostics, version);
    client.send(Message::Notification(Notification {
        method: PublishDiagnostics::METHOD.to_string(),
        params: serde_json::to_value(params).unwrap(),
    }))?;
    Ok(())
}
