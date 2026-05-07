use crossbeam_channel::Sender;
use greycat_analyzer_core::{Document, SourceManager};
use log::debug;
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
                // TODO
                // let ws_root = Path::new(ws.uri.as_str());
                // let project_path = ws_root.join("project.gcl");
                // if project_path.exists() {
                //     let uri = Uri::from_str(project_path.to_str().unwrap())?;
                //     self.register_project(uri);
                // }
            }
        }
        Ok(())
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
