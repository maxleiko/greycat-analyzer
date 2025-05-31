use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use log::debug;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::{Document, Project};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Backend {
    client: Client,
    documents: Arc<DashMap<Url, Document>>,
    projects: Arc<DashMap<Url, Project>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Default::default(),
            projects: Default::default(),
        }
    }

    pub fn register_project(&self, project_path: PathBuf) {
        let uri = Url::from_file_path(&project_path).expect("invalid url");
        let project = Project::new(project_path);
        debug!("new project {project}");
        self.projects.insert(uri, project);
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "greycat-analyzer-ls".into(),
                version: Some(VERSION.into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                format!("greycat-analyzer-ls v{VERSION} initialized"),
            )
            .await;

        if let Ok(Some(workspaces)) = self.client.workspace_folders().await {
            for ws in workspaces {
                if let Ok(ws_root) = ws.uri.to_file_path() {
                    let project_path = ws_root.join("project.gcl");
                    if project_path.exists() {
                        self.register_project(project_path);
                    }
                }
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("did_open {}", params.text_document.uri);
        let doc = params.text_document;
        match self.projects.get(&doc.uri) {
            Some(_project) => {
                // already registered, nothing to do
            }
            None => {
                if let Ok(path) = doc.uri.to_file_path() {
                    if path.file_name() == Some(OsStr::new("project.gcl")) {
                        // the document is a project.gcl file, lets register it
                        self.register_project(path);
                        return;
                    }
                }
                // if we reach this point it means we are dealing with a module
                // that may or may not be a part of a project, lets try to find
                // if we have projects that include that module
                let orphan = self
                    .projects
                    .iter()
                    .filter(|project| project.includes(&doc.uri))
                    .count()
                    == 0;
                if orphan {
                    self.client
                        .publish_diagnostics(
                            doc.uri,
                            vec![Diagnostic::new_simple(
                                Range::default(),
                                "This module is not part of any registered project".to_string(),
                            )],
                            Some(doc.version),
                        )
                        .await;
                }
            }
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(mut doc) = self.documents.get_mut(&params.text_document.uri) {
            doc.apply_changes(params.content_changes, params.text_document.version);
            debug!("did_change {}", &*doc);
        } else {
            debug!("did_change unknown: {}", params.text_document.uri);
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(doc) = self.documents.get_mut(&params.text_document.uri) {
            debug!("did_save {}", &*doc);
        } else {
            debug!("did_save unknown: {}", params.text_document.uri);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        debug!("did_close {}", params.text_document.uri);
        self.documents.remove(&params.text_document.uri);
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
