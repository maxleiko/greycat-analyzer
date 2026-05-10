use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Sender;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::module_desc::parse_module_desc;
use greycat_analyzer_core::registry::RegistryFetcher;
use greycat_analyzer_core::resolver::FsContext;
use greycat_analyzer_core::{
    Document, SourceManager,
    diagnostics::{parse_diagnostics, pragma_diagnostics},
};
use log::{debug, info, warn};
use lsp_server::*;
use lsp_types::{
    notification::{Notification as _, PublishDiagnostics},
    request::{RegisterCapability, Request as _},
    *,
};

use crate::Result;
use crate::capabilities::diagnostics_from_module;

/// Threshold above which a per-edit rebuild is surfaced as a one-line
/// `info!` so users see hot spots without flooding the log on healthy
/// edits. Tuned by hand — typical stdlib rebuilds land at ~30-100ms,
/// so 500ms catches genuine outliers.
const SLOW_REBUILD_MS: u128 = 500;

pub struct Backend {
    pub client: Sender<Message>,
    pub manager: SourceManager,
    pub project_analysis: ProjectAnalysis,
    // P15.5 — used to anchor `@include` / `@library` pragma diagnostics.
    /// Project root (parent of `project.gcl`) captured at workspace
    /// load time.
    pub project_root: Option<PathBuf>,
    /// When `true`, lint diagnostics from non-project modules
    /// (`lib/<name>/...`) are surfaced in the editor too. Driven by
    /// the `greycat-analyzer.lintLibs` extension setting via the
    /// LSP's `initializationOptions`. Default `false` — most users
    /// don't want warnings about vendored library code.
    pub lint_libs: bool,
    // P15.3
    /// Registry fetcher for `@library` version completion.
    /// `None` skips the lazy resolution path and surfaces the
    /// placeholder verbatim (the foundational shape used by tests
    /// and the WASM bridge until the playground wires its own
    /// JS-side fetcher).
    pub registry: Option<Arc<dyn RegistryFetcher>>,
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

    /// Pull cached parse + semantic + lint diagnostics for `uri` and
    /// push them to the client. The cache is populated by
    /// [`ProjectAnalysis::analyze`] on workspace load and by
    /// [`ProjectAnalysis::invalidate`] on every `did_open` / `did_change`.
    fn publish_for(&self, uri: &Uri) -> Result<()> {
        let Some(cell) = self.manager.get(uri) else {
            return Ok(());
        };
        let doc = cell.borrow();
        let mut diags = parse_diagnostics(doc.root_node(), &doc.text);
        if let Some(module) = self.project_analysis.module(uri) {
            diags.extend(diagnostics_from_module(&doc.text, module, self.lint_libs));
        }
        // P15.5 — pragma resolution diagnostics. Recomputed on every
        // publish so edits to `@include` / `@library` pragmas reflect
        // immediately. Skipped when no project root is known (single-
        // file mode).
        if let Some(root) = self.project_root.as_ref() {
            let desc = parse_module_desc(uri.clone(), &doc.text, doc.root_node());
            if let Ok(ctx) = FsContext::new() {
                diags.extend(pragma_diagnostics(&doc.text, &desc, root, &ctx));
            }
        }
        self.publish_diagnostics(uri.clone(), diags, Some(doc.version))
    }

    pub fn initialized(&mut self, init: &InitializeParams) -> Result<()> {
        // Pull `lintLibs` (and any future settings) out of
        // `initializationOptions` before walking workspace folders so
        // the very first round of `publish_for` already honors the
        // user's preference. Missing / malformed payload silently
        // falls back to the default (`false`) — no need to fail the
        // handshake over an absent option.
        if let Some(opts) = init.initialization_options.as_ref()
            && let Some(b) = opts.get("lintLibs").and_then(|v| v.as_bool())
        {
            self.lint_libs = b;
            debug!("[init] lintLibs={b}");
        }
        if let Some(workspaces) = init.workspace_folders.as_ref() {
            debug!("workspaces:");
            for ws in workspaces {
                debug!("- {}={}", ws.name, ws.uri.as_str());
                self.load_workspace(&ws.uri);
            }
        }
        // **P19.22** — register a workspace file watcher so external
        // tools (notably `greycat install` populating `lib/installed`
        // and `lib/<name>/...`) trigger a project reload. Mirrors the
        // TS reference's index.ts:294 registration. Gated on the
        // client supporting dynamic registration of watched files.
        let supports_watch = init
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        if supports_watch {
            self.register_file_watchers()?;
        } else {
            debug!("[init] client does not support didChangeWatchedFiles dynamic registration");
        }
        Ok(())
    }

    /// Send `client/registerCapability` to ask the editor to forward
    /// filesystem create/change/delete events for `*.gcl` files and
    /// `lib/installed` manifests. The response handler in `server.rs`
    /// just logs it — registration is fire-and-forget from our side.
    fn register_file_watchers(&self) -> Result<()> {
        let watchers = vec![
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.gcl".into()),
                kind: None, // default = create | change | delete
            },
            FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/lib/installed".into()),
                kind: None,
            },
        ];
        let registration = Registration {
            id: "greycat-analyzer-file-watch".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: Some(serde_json::to_value(
                DidChangeWatchedFilesRegistrationOptions { watchers },
            )?),
        };
        let params = RegistrationParams {
            registrations: vec![registration],
        };
        // Use a deterministic id so we can recognize the response in
        // the main loop's debug log even if multiple registrations land.
        let req = Request {
            id: RequestId::from("register-file-watch".to_string()),
            method: RegisterCapability::METHOD.into(),
            params: serde_json::to_value(params)?,
        };
        self.client.send(Message::Request(req))?;
        Ok(())
    }

    // P1.4 — typed diagnostic publication lands here.
    /// Resolve a workspace-folder URI to a local path, look for
    /// `project.gcl` at its root, and recursively load every reachable
    /// module via [`SourceManager::load_project`]. Errors are logged but
    /// don't fail the LSP handshake.
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
        self.project_root = Some(ws_root.clone());
        let load_start = Instant::now();
        let report = self.manager.load_project(&project_file);
        let load_took = load_start.elapsed();
        for lib in &report.unresolved_libraries {
            warn!("unresolved @library('{lib}') in {}", project_file.display());
        }
        for err in &report.errors {
            warn!("[load_project] {err}");
        }
        // Single project-wide pipeline pass over everything we just
        // loaded — the per-doc analyses land in the cache.
        let rebuild_start = Instant::now();
        self.project_analysis.rebuild(&self.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[load_project] {} files in {load_took:?} (parse+load) + {rebuild_took:?} (analyze) from {}",
            report.loaded.len(),
            project_file.display()
        );
        for (uri, _) in report.loaded {
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
        self.invalidate_with_slow_warning(&uri, "did_open");
        self.publish_for(&uri)?;
        Ok(())
    }

    pub fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let doc = self
            .manager
            .update(&uri, params.content_changes, params.text_document.version);
        debug!("[did_change] {doc}");
        drop(doc);
        // **P19.22** — pragma edits to the project entrypoint can add /
        // remove `@library` / `@include`, which changes the closure of
        // reachable modules. The text update above only refreshes
        // `project.gcl`'s own bytes; the resolver doesn't notice the
        // new modules until we re-walk. Detect entrypoint edits and
        // re-walk before invalidating so the rebuild sees the full
        // closure.
        if self.is_project_entrypoint(&uri) {
            self.reload_project_closure();
        } else {
            self.invalidate_with_slow_warning(&uri, "did_change");
        }
        self.publish_for(&uri)?;
        Ok(())
    }

    /// True when `uri` resolves to `<project_root>/project.gcl`. Used
    /// to gate the re-walk on `did_change` and to dispatch `lib/installed`
    /// / `*.gcl` events from the workspace watcher to the right project.
    fn is_project_entrypoint(&self, uri: &Uri) -> bool {
        let Some(root) = self.project_root.as_ref() else {
            return false;
        };
        let entrypoint = root.join("project.gcl");
        let Ok(canonical) = entrypoint.canonicalize() else {
            return false;
        };
        let Some(path) = uri_to_path(uri) else {
            return false;
        };
        path.canonicalize().map(|p| p == canonical).unwrap_or(false)
    }

    /// Re-walk the project entrypoint's `@library` / `@include`
    /// closure against the current in-memory `project.gcl` and rebuild
    /// the analysis. Idempotent: `SourceManager::load_file` skips
    /// already-loaded files (including the in-editor entrypoint), so
    /// only newly-referenced modules get parsed. Triggered from
    /// `did_change` on the entrypoint and from the `lib/installed`
    /// branch of the workspace watcher.
    fn reload_project_closure(&mut self) {
        let Some(root) = self.project_root.as_ref() else {
            return;
        };
        let project_file = root.join("project.gcl");
        let load_start = Instant::now();
        let report = self.manager.load_project(&project_file);
        let load_took = load_start.elapsed();
        for lib in &report.unresolved_libraries {
            warn!("unresolved @library('{lib}') in {}", project_file.display());
        }
        for err in &report.errors {
            warn!("[reload_project] {err}");
        }
        // **P19.23** — evict modules that are no longer in the
        // project's reachable closure (e.g., the user commented out an
        // `@library` line). Without this, types the lib exposed would
        // still resolve and the editor would silently miss the
        // resulting "unknown type" errors. `evict_unreachable` skips
        // opened buffers — those stay until `did_close` / explicit
        // user action.
        #[allow(clippy::mutable_key_type)] // lsp_types::Uri as a HashSet key is fine in practice.
        let reachable: std::collections::HashSet<Uri> = report.reachable.iter().cloned().collect();
        let evicted = self.manager.evict_unreachable(&reachable);
        if !evicted.is_empty() {
            info!(
                "[reload_project] evicted {} unreachable file(s):",
                evicted.len()
            );
            for uri in &evicted {
                info!("[reload_project]   - {}", uri.as_str());
            }
        }
        // Log additions explicitly too — symmetric with eviction so the
        // user can see project-closure changes in the LSP output.
        if !report.loaded.is_empty() {
            info!(
                "[reload_project] loaded {} new file(s):",
                report.loaded.len()
            );
            for (uri, _) in &report.loaded {
                info!("[reload_project]   + {}", uri.as_str());
            }
        }
        let rebuild_start = Instant::now();
        self.project_analysis.rebuild(&self.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[reload_project] {load_took:?} (parse+load) + {rebuild_took:?} (analyze) — closure: {} reachable, {} added, {} evicted",
            report.reachable.len(),
            report.loaded.len(),
            evicted.len(),
        );
        // Clear diagnostics for evicted URIs so the editor's stale
        // entries disappear.
        for uri in &evicted {
            if let Err(e) = self.publish_diagnostics(uri.clone(), Vec::new(), None) {
                warn!("clear-diagnostics({}) failed: {e}", uri.as_str());
            }
        }
        // Republish for every newly-loaded module so the editor sees
        // their diagnostics immediately. Also republish for the rest of
        // the reachable closure since the rebuild may have changed
        // diagnostics on files that were already loaded (e.g., the
        // project entrypoint goes from "all good" to "unknown library"
        // when an `@library` line is commented out).
        for uri in &report.reachable {
            if let Err(e) = self.publish_for(uri) {
                warn!("publish_for({}) failed: {e}", uri.as_str());
            }
        }
    }

    /// Wrap [`ProjectAnalysis::invalidate`] with a wall-clock measure
    /// and surface an `info!` line only when the rebuild exceeds
    /// [`SLOW_REBUILD_MS`]. Healthy edits stay quiet at the default
    /// log level; outliers show up so users can spot project-side hot
    /// spots without flipping into `debug`.
    fn invalidate_with_slow_warning(&mut self, uri: &Uri, source: &str) {
        let start = Instant::now();
        self.project_analysis.invalidate(&self.manager, uri);
        let took = start.elapsed();
        if took.as_millis() >= SLOW_REBUILD_MS {
            info!("[slow-rebuild] {source} for {} took {took:?}", uri.as_str());
        }
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

    // P19.22
    /// Workspace file watcher events. Two flavors of
    /// trigger matter to us:
    ///
    /// - `lib/installed` — written by `greycat install` after it
    ///   downloads a library into `lib/<name>/`. We can't know in
    ///   advance which files appeared; the simplest correct response
    ///   is to re-walk the project closure (which `load_project` does
    ///   idempotently — already-loaded files are preserved).
    ///
    /// - `*.gcl` outside the editor — files created or deleted by
    ///   external tools (a `git pull` adding modules, `greycat install`
    ///   replacing a stdlib version, etc.). Created → load + reload
    ///   the project closure (in case it's reachable through a
    ///   pragma); deleted → drop from the manager + reload.
    ///
    /// In-editor edits (`did_change`) are NOT routed through this path
    /// — they go through the textDocument flow.
    pub fn did_change_watched_files(&mut self, params: DidChangeWatchedFilesParams) -> Result<()> {
        if params.changes.is_empty() {
            return Ok(());
        }
        let mut needs_reload = false;
        for ev in &params.changes {
            let uri = &ev.uri;
            let path = match uri_to_path(uri) {
                Some(p) => p,
                None => continue,
            };
            let is_installed = path.file_name().and_then(|n| n.to_str()) == Some("installed")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    == Some("lib");
            let is_gcl = path.extension().and_then(|e| e.to_str()) == Some("gcl");
            if is_installed {
                debug!("[watch] {:?} on lib/installed -> reload", ev.typ);
                needs_reload = true;
                continue;
            }
            if !is_gcl {
                continue;
            }
            // Skip files the editor owns the live state of — `did_change`
            // already keeps those fresh. Only refresh closed sources.
            let opened = self
                .manager
                .get(uri)
                .map(|cell| cell.borrow().opened)
                .unwrap_or(false);
            if opened {
                debug!("[watch] {:?} on opened {} -> skip", ev.typ, uri.as_str());
                continue;
            }
            match ev.typ {
                FileChangeType::CREATED => {
                    debug!("[watch] created {}", uri.as_str());
                    needs_reload = true;
                }
                FileChangeType::CHANGED => {
                    // Refresh the closed source from disk. A change to a
                    // non-pragma module doesn't need a project re-walk;
                    // an `invalidate` is enough.
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        self.manager.add_simple(uri.clone(), text, "project", false);
                        self.invalidate_with_slow_warning(uri, "watch");
                        if let Err(e) = self.publish_for(uri) {
                            warn!("publish_for({}) failed: {e}", uri.as_str());
                        }
                    }
                }
                FileChangeType::DELETED => {
                    debug!("[watch] deleted {}", uri.as_str());
                    let _ = self.manager.remove(uri);
                    self.publish_diagnostics(uri.clone(), Vec::new(), None)?;
                    needs_reload = true;
                }
                _ => {}
            }
        }
        if needs_reload {
            self.reload_project_closure();
        }
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
