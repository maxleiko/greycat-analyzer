use std::sync::Arc;

use dashmap::DashMap;
use log::debug;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::Document;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Backend {
    client: Client,
    documents: Arc<DashMap<Url, Document>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Default::default(),
        }
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
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        debug!("did_open {}", params.text_document.uri);
        let doc = params.text_document;
        self.documents.insert(doc.uri.clone(), Document::from(doc));
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(mut doc) = self.documents.get_mut(&params.text_document.uri) {
            doc.apply_changes(params.content_changes, params.text_document.version);
            debug!("did_change {}", &*doc);
        } else {
            debug!("did_change unknown: {}", params.text_document.uri);
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
