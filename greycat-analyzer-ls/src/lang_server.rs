use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crossbeam_channel::Sender;
use dashmap::DashMap;
use dashmap::mapref::one::RefMut;
use log::debug;
use lsp_server::Notification;
use lsp_server::*;
use lsp_types::notification::DidChangeTextDocument;
use lsp_types::notification::DidCloseTextDocument;
use lsp_types::notification::DidOpenTextDocument;
use lsp_types::notification::DidSaveTextDocument;
use lsp_types::notification::Notification as _;
use lsp_types::notification::PublishDiagnostics;
use lsp_types::request::GotoDefinition;
use lsp_types::*;

use crate::{Document, Project};

const VERSION: &str = env!("CARGO_PKG_VERSION");

type AnyError = Box<dyn std::error::Error + Send + Sync>;

pub fn start_server() -> Result<(), AnyError> {
    let running = Arc::new(AtomicBool::new(true));

    let r = Arc::clone(&running);
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    let (conn, io_threads) = Connection::stdio();

    let (id, params) = conn.initialize_start_while(|| running.load(Ordering::SeqCst))?;
    let init_params: InitializeParams = serde_json::from_value(params).unwrap();

    let initialize_data = serde_json::json!({
        "serverInfo": {
            "name": "greycat-analyzer",
            "version": VERSION
        },
        "capabilities": ServerCapabilities {
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
    });

    conn.initialize_finish_while(id, initialize_data, || running.load(Ordering::SeqCst))?;
    main_loop(conn, init_params)?;
    io_threads.join()?;

    debug!("shutting down greycat-analyzer");
    Ok(())
}

fn main_loop(conn: Connection, init: InitializeParams) -> Result<(), AnyError> {
    debug!("starting example main loop");
    let mut server = LspServer {
        init,
        client: conn.sender.clone(),
        documents: Default::default(),
        projects: Default::default(),
    };

    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }

                eprintln!("got request: {req:?}");
                match cast_req::<GotoDefinition>(req) {
                    Ok((id, params)) => {
                        debug!("got gotoDefinition request #{id}: {params:?}");
                        let result = Some(GotoDefinitionResponse::Array(Vec::new()));
                        let result = serde_json::to_value(&result).unwrap();
                        let resp = Response {
                            id,
                            result: Some(result),
                            error: None,
                        };
                        conn.sender.send(Message::Response(resp))?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => panic!("{err:?}"),
                    Err(ExtractError::MethodMismatch(req)) => req,
                };
                // ...
            }
            Message::Response(resp) => {
                debug!("got response: {resp:?}");
            }
            Message::Notification(not) => match not.method.as_str() {
                DidOpenTextDocument::METHOD => {
                    server.did_open(not.extract(DidOpenTextDocument::METHOD)?)?
                }
                DidChangeTextDocument::METHOD => {
                    server.did_change(not.extract(DidChangeTextDocument::METHOD)?)?
                }
                DidSaveTextDocument::METHOD => {
                    server.did_save(not.extract(DidSaveTextDocument::METHOD)?)?
                }
                DidCloseTextDocument::METHOD => {
                    server.did_close(not.extract(DidCloseTextDocument::METHOD)?)?
                }
                _ => {
                    debug!("got notification: {not:#?}");
                }
            },
        }
    }
    Ok(())
}

struct LspServer {
    init: InitializeParams,
    client: Sender<Message>,
    documents: Arc<DashMap<Uri, Document>>,
    projects: Arc<DashMap<Uri, Project>>,
}

impl LspServer {
    pub fn register_project(&self, uri: Uri) -> RefMut<Uri, Project> {
        let project = Project::new(PathBuf::from(uri.as_str()));
        debug!("new project {project}");
        self.projects.insert(uri.clone(), project);
        self.projects.get_mut(&uri).unwrap()
    }

    fn initialized(&self, _params: InitializedParams) -> Result<(), AnyError> {
        if let Some(workspaces) = self.init.workspace_folders.as_ref() {
            for ws in workspaces {
                let ws_root = Path::new(ws.uri.as_str());
                let project_path = ws_root.join("project.gcl");
                if project_path.exists() {
                    let uri = Uri::from_str(project_path.to_str().unwrap())?;
                    self.register_project(uri);
                }
            }
        }
        Ok(())
    }

    fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Result<(), AnyError> {
        debug!("did_open {}", params.text_document.uri.as_str());
        let doc = params.text_document;
        match self.projects.get(&doc.uri) {
            Some(_project) => {
                // already registered, nothing to do
                debug!("already registered {}, nothing to do", doc.uri.as_str());
            }
            None => {
                let path = Path::new(doc.uri.as_str());
                if path.file_name() == Some(OsStr::new("project.gcl")) {
                    // the document is a project.gcl file, lets register it
                    let mut project = self.register_project(doc.uri.clone());
                    project
                        .value_mut()
                        .add_module(doc.uri.clone(), Document::from(doc));
                    return Ok(());
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
                    let params = PublishDiagnosticsParams::new(
                        doc.uri,
                        vec![Diagnostic::new_simple(
                            Range::default(),
                            "This module is not part of any registered project".to_string(),
                        )],
                        Some(doc.version),
                    );
                    self.client.send(Message::Notification(Notification {
                        method: PublishDiagnostics::METHOD.to_string(),
                        params: serde_json::to_value(params).unwrap(),
                    }))?;
                }
            }
        }

        Ok(())
    }

    fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<(), AnyError> {
        if let Some(mut doc) = self.documents.get_mut(&params.text_document.uri) {
            doc.apply_changes(params.content_changes, params.text_document.version);
            debug!("did_change {}", &*doc);
        } else {
            debug!("did_change unknown: {}", params.text_document.uri.as_str());
        }
        Ok(())
    }

    fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Result<(), AnyError> {
        if let Some(doc) = self.documents.get_mut(&params.text_document.uri) {
            debug!("did_save {}", &*doc);
        } else {
            debug!("did_save unknown: {}", params.text_document.uri.as_str());
        }
        Ok(())
    }

    fn did_close(&mut self, params: DidCloseTextDocumentParams) -> Result<(), AnyError> {
        debug!("did_close {}", params.text_document.uri.as_str());
        self.documents.remove(&params.text_document.uri);
        Ok(())
    }
}

fn cast_req<R>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}
