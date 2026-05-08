use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use log::debug;
use lsp_server::*;
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::request::{
    CodeActionRequest, DocumentHighlightRequest, DocumentSymbolRequest, FoldingRangeRequest,
    Formatting, GotoDefinition, HoverRequest, InlayHintRequest, PrepareRenameRequest,
    RangeFormatting, References, Rename, SelectionRangeRequest, SemanticTokensFullRequest,
    SignatureHelpRequest, WorkspaceSymbolRequest,
};
use lsp_types::*;

use crate::Result;
use crate::backend::Backend;
use crate::capabilities;

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
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            signature_help_provider: Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), ",".into()]),
                retrigger_characters: None,
                work_done_progress_options: Default::default(),
            }),
            definition_provider: Some(OneOf::Left(true)),
            implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
            references_provider: Some(OneOf::Left(true)),
            document_highlight_provider: Some(OneOf::Left(true)),
            document_symbol_provider: Some(OneOf::Left(true)),
            workspace_symbol_provider: Some(OneOf::Left(true)),
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            rename_provider: Some(OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: Default::default(),
            })),
            folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
            selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
            document_formatting_provider: Some(OneOf::Left(true)),
            document_range_formatting_provider: Some(OneOf::Left(true)),
            inlay_hint_provider: Some(OneOf::Left(true)),
            semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
                SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: capabilities::SEMANTIC_TOKEN_TYPES.to_vec(),
                        token_modifiers: vec![],
                    },
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    work_done_progress_options: Default::default(),
                },
            )),
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
        project_analysis: Default::default(),
    };

    server.initialized(&init)?;

    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                debug!("got request: {req:?}");
                if let Some(response) = handle_request(&server, req) {
                    conn.sender.send(Message::Response(response))?;
                }
            }
            Message::Response(resp) => debug!("got response: {resp:?}"),
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
                _ => debug!("got notification: {not:#?}"),
            },
        }
    }
    Ok(())
}

fn handle_request(server: &Backend, req: Request) -> Option<Response> {
    let req = match try_handle::<HoverRequest, _, _>(server, req, hover_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<SignatureHelpRequest, _, _>(server, req, signature_help_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<GotoDefinition, _, _>(server, req, goto_definition_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<lsp_types::request::GotoImplementation, _, _>(
        server,
        req,
        goto_implementation_handler,
    ) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<DocumentSymbolRequest, _, _>(server, req, document_symbols_handler)
    {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<References, _, _>(server, req, references_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<PrepareRenameRequest, _, _>(server, req, prepare_rename_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<Rename, _, _>(server, req, rename_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req =
        match try_handle::<DocumentHighlightRequest, _, _>(server, req, document_highlight_handler)
        {
            Ok(resp) => return Some(resp),
            Err(req) => req,
        };
    let req = match try_handle::<SelectionRangeRequest, _, _>(server, req, selection_ranges_handler)
    {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<FoldingRangeRequest, _, _>(server, req, folding_ranges_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<CodeActionRequest, _, _>(server, req, code_actions_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<InlayHintRequest, _, _>(server, req, inlay_hints_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<Formatting, _, _>(server, req, formatting_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req = match try_handle::<RangeFormatting, _, _>(server, req, range_formatting_handler) {
        Ok(resp) => return Some(resp),
        Err(req) => req,
    };
    let req =
        match try_handle::<WorkspaceSymbolRequest, _, _>(server, req, workspace_symbols_handler) {
            Ok(resp) => return Some(resp),
            Err(req) => req,
        };
    let _req =
        match try_handle::<SemanticTokensFullRequest, _, _>(server, req, semantic_tokens_handler) {
            Ok(resp) => return Some(resp),
            Err(req) => req,
        };
    None
}

fn try_handle<R, F, T>(
    server: &Backend,
    req: Request,
    handler: F,
) -> std::result::Result<Response, Request>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
    R::Result: serde::Serialize,
    F: Fn(&Backend, R::Params) -> T,
    T: serde::Serialize,
{
    let cloned = req.clone();
    match req.extract::<R::Params>(R::METHOD) {
        Ok((id, params)) => {
            let result = handler(server, params);
            let result = serde_json::to_value(&result).unwrap();
            Ok(Response {
                id,
                result: Some(result),
                error: None,
            })
        }
        Err(ExtractError::JsonError { .. }) => Err(cloned),
        Err(ExtractError::MethodMismatch(req)) => Err(req),
    }
}

// =============================================================================
// Per-capability handlers — pull the relevant `Document` out of the manager
// and forward to `capabilities`.
// =============================================================================

fn hover_handler(server: &Backend, params: HoverParams) -> Option<Hover> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let cell = server.manager.get(&uri)?;
    let doc = cell.borrow();
    capabilities::hover(&doc.text, &doc.lib, doc.root_node(), pos)
}

fn signature_help_handler(server: &Backend, params: SignatureHelpParams) -> Option<SignatureHelp> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let cell = server.manager.get(&uri)?;
    let doc = cell.borrow();
    capabilities::signature_help(&doc.text, &doc.lib, doc.root_node(), pos)
}

fn goto_definition_handler(
    server: &Backend,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    use greycat_analyzer_analysis::resolver::Definition;

    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let cell = server.manager.get(&uri)?;
    let doc = cell.borrow();
    if let Some(loc) =
        capabilities::goto_definition(&doc.text, &doc.lib, doc.root_node(), &uri, pos)
    {
        return Some(loc);
    }
    // P11.3: cross-module fallback. Consult the cached resolutions —
    // if the cursor binds to a `Definition::ProjectDecl`, resolve the
    // foreign module's decl-name range out of the project analysis
    // cache and source manager.
    let module = server.project_analysis.module(&uri)?;
    let cursor_idx = capabilities::cursor_ident_idx(&doc.text, doc.root_node(), pos, &module.hir)?;
    if let Some(Definition::ProjectDecl {
        uri: foreign_uri,
        decl,
    }) = module.resolutions.lookup(cursor_idx)
    {
        drop(doc);
        let foreign_module = server.project_analysis.module(&foreign_uri)?;
        let foreign_cell = server.manager.get(&foreign_uri)?;
        let foreign_doc = foreign_cell.borrow();
        return capabilities::cross_module_decl_location(
            &foreign_uri,
            &foreign_doc.text,
            &foreign_module.hir,
            decl,
        )
        .map(GotoDefinitionResponse::Scalar);
    }
    // P11.5: cross-module member access (`a.b` where the receiver type
    // lives in another module). Consult the cached `foreign_member_uses`.
    let foreign = module.analysis.foreign_member_lookup(cursor_idx)?;
    let foreign_uri = foreign.uri.clone();
    let member = foreign.member;
    drop(doc);
    let foreign_module = server.project_analysis.module(&foreign_uri)?;
    let foreign_cell = server.manager.get(&foreign_uri)?;
    let foreign_doc = foreign_cell.borrow();
    capabilities::cross_module_member_location(
        &foreign_uri,
        &foreign_doc.text,
        &foreign_module.hir,
        &member,
    )
    .map(GotoDefinitionResponse::Scalar)
}

fn goto_implementation_handler(
    server: &Backend,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    // P11.6: walk every cached module's TypeDecl methods (not just the
    // current module) — falls through to in-module goto_implementation
    // → goto_definition when there's no method match.
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    capabilities::goto_implementation_across_project(
        &server.project_analysis,
        &server.manager,
        &uri,
        pos,
    )
}

fn document_symbols_handler(
    server: &Backend,
    params: DocumentSymbolParams,
) -> Option<DocumentSymbolResponse> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    let syms = capabilities::document_symbols(&doc.text, &doc.lib, doc.root_node());
    Some(DocumentSymbolResponse::Nested(syms))
}

fn references_handler(server: &Backend, params: ReferenceParams) -> Option<Vec<Location>> {
    // P11.4: scope-aware project-wide references via cached resolutions.
    // Walks every cached `ModuleAnalysis::resolutions.uses` and matches
    // by `Definition::Decl` (home module) / `Definition::ProjectDecl`
    // (importers) — replaces the prior text-equality fallback.
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    Some(capabilities::references_across_project(
        &server.project_analysis,
        &server.manager,
        &uri,
        pos,
    ))
}

fn prepare_rename_handler(
    server: &Backend,
    params: TextDocumentPositionParams,
) -> Option<PrepareRenameResponse> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    capabilities::prepare_rename(&doc.text, doc.root_node(), params.position)
}

fn rename_handler(server: &Backend, params: RenameParams) -> Option<WorkspaceEdit> {
    // P11.4: scope-aware project-wide rename via cached resolutions.
    let uri = params.text_document_position.text_document.uri;
    let pos = params.text_document_position.position;
    capabilities::rename_across_project(
        &server.project_analysis,
        &server.manager,
        &uri,
        pos,
        &params.new_name,
    )
}

fn document_highlight_handler(
    server: &Backend,
    params: DocumentHighlightParams,
) -> Option<Vec<DocumentHighlight>> {
    let uri = params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;
    let cell = server.manager.get(&uri)?;
    let doc = cell.borrow();
    Some(capabilities::document_highlights(
        &doc.text,
        doc.root_node(),
        pos,
    ))
}

fn selection_ranges_handler(
    server: &Backend,
    params: SelectionRangeParams,
) -> Option<Vec<SelectionRange>> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    Some(capabilities::selection_ranges(
        &doc.text,
        doc.root_node(),
        &params.positions,
    ))
}

fn folding_ranges_handler(
    server: &Backend,
    params: FoldingRangeParams,
) -> Option<Vec<FoldingRange>> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    Some(capabilities::folding_ranges(&doc.text, doc.root_node()))
}

fn code_actions_handler(server: &Backend, params: CodeActionParams) -> Option<CodeActionResponse> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    Some(capabilities::code_actions(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        &params.text_document.uri,
        params.range,
    ))
}

fn inlay_hints_handler(server: &Backend, params: InlayHintParams) -> Option<Vec<InlayHint>> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    Some(capabilities::inlay_hints(
        &doc.text,
        &doc.lib,
        doc.root_node(),
        &params.range,
    ))
}

fn formatting_handler(server: &Backend, params: DocumentFormattingParams) -> Option<Vec<TextEdit>> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    capabilities::formatting(&doc.text, doc.root_node())
}

fn range_formatting_handler(
    server: &Backend,
    params: DocumentRangeFormattingParams,
) -> Option<Vec<TextEdit>> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    capabilities::range_formatting(&doc.text, doc.root_node(), params.range)
}

fn workspace_symbols_handler(
    server: &Backend,
    params: WorkspaceSymbolParams,
) -> Option<Vec<WorkspaceSymbol>> {
    let docs: Vec<(Uri, String, String)> = server
        .manager
        .iter()
        .map(|(uri, cell)| {
            let d = cell.borrow();
            (uri.clone(), d.lib.clone(), d.text.clone())
        })
        .collect();
    Some(capabilities::workspace_symbols(docs, &params.query))
}

fn semantic_tokens_handler(
    server: &Backend,
    params: SemanticTokensParams,
) -> Option<SemanticTokensResult> {
    let cell = server.manager.get(&params.text_document.uri)?;
    let doc = cell.borrow();
    Some(SemanticTokensResult::Tokens(capabilities::semantic_tokens(
        &doc.text,
        &doc.lib,
        doc.root_node(),
    )))
}
