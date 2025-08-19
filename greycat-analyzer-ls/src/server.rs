use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::debug;
use lsp_server::*;
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::request::GotoDefinition;
use lsp_types::*;

use crate::Result;
use crate::backend::Backend;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn start_server() -> Result<()> {
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

fn main_loop(conn: Connection, init: InitializeParams) -> Result<()> {
    debug!("starting main loop");
    let mut server = Backend {
        client: conn.sender.clone(),
        manager: Default::default(),
    };

    server.initialized(&init)?;

    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }

                debug!("got request: {req:?}");
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

fn cast_req<R>(req: Request) -> std::result::Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}
