use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use greycat_analyzer_core::{Document, SourceManager};
use log::{debug, warn};
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
    }

    pub fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let doc = Document::new(params.text_document);
        debug!("[did_open] {doc}");
        self.manager.add(doc);
        Ok(())
    }

    pub fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        let doc = self.manager.update(
            &params.text_document.uri,
            params.content_changes,
            params.text_document.version,
        );
        debug!("[did_change] {doc}");
        Ok(())
    }

    pub fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Result<()> {
        debug!(
            "[did_save] {} (text={:?})",
            params.text_document.uri.as_str(),
            params.text
        );
        Ok(())
    }

    pub fn did_close(&mut self, params: DidCloseTextDocumentParams) -> Result<()> {
        debug!("[did_close] {}", params.text_document.uri.as_str());
        // self.manager.remove(&params.text_document.uri);
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
