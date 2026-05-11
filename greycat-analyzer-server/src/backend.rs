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
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Result;
use crate::capabilities::diagnostics_from_module;

/// Threshold above which a per-edit rebuild is surfaced as a one-line
/// `info!` so users see hot spots without flooding the log on healthy
/// edits. Tuned by hand — typical stdlib rebuilds land at ~30-100ms,
/// so 500ms catches genuine outliers.
const SLOW_REBUILD_MS: u128 = 500;

// P32.1
/// One independent GreyCat project: a `project.gcl` plus the
/// `@library`/`@include` closure it pulls in, with its own
/// [`SourceManager`] and [`ProjectAnalysis`] so two projects'
/// type arenas / decl namespaces never collide.
pub struct Project {
    /// Directory holding the `project.gcl` entrypoint. Kept exactly as
    /// it came in via `uri_to_path` (no eager canonicalize) — comparisons
    /// canonicalize on demand, see [`Backend::is_project_entrypoint`].
    pub root: PathBuf,
    pub manager: SourceManager,
    pub analysis: ProjectAnalysis,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            manager: SourceManager::new(),
            analysis: ProjectAnalysis::new(),
        }
    }
}

// P32.1
/// LSP server state.
///
/// Storage shape is multi-project: `projects` keyed by project-root
/// path, `uri_owner` mapping every loaded document to its owning
/// project, `workspace_roots` capturing each `workspace/didChangeWorkspaceFolders`
/// entry (used by the parent-walk in P32.3).
///
/// Two server-wide knobs (`lint_libs`, `registry`) live here instead
/// of on each project — `initializationOptions` is one-shot and the
/// registry fetcher is the same regardless of project.
pub struct Backend {
    pub client: Sender<Message>,
    /// All loaded projects, keyed by their root directory.
    pub projects: FxHashMap<PathBuf, Project>,
    /// URI → owning-project root. Populated eagerly during
    /// `load_workspace` and on `did_open` for files that aren't part
    /// of any project's closure.
    pub uri_owner: FxHashMap<Uri, PathBuf>,
    /// Workspace folder roots from `initialize` (and later
    /// `workspace/didChangeWorkspaceFolders`). Bounds the parent-walk
    /// in P32.3's owner-search.
    pub workspace_roots: Vec<PathBuf>,
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
    // P32.1
    /// Look up the [`Project`] that owns `uri`. Returns `None` when
    /// the URI hasn't been bound to a project yet (P32.3 will fill
    /// that in via the parent-walk; P32.5 will surface it as an
    /// orphan).
    pub fn project_for(&self, uri: &Uri) -> Option<&Project> {
        let root = self.uri_owner.get(uri)?;
        self.projects.get(root)
    }

    pub fn project_for_mut(&mut self, uri: &Uri) -> Option<&mut Project> {
        let root = self.uri_owner.get(uri)?.clone();
        self.projects.get_mut(&root)
    }

    /// Bind `uri` to `root` in `uri_owner`. Idempotent.
    fn bind_uri(&mut self, uri: Uri, root: PathBuf) {
        self.uri_owner.insert(uri, root);
    }

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
        let Some(project) = self.project_for(uri) else {
            return Ok(());
        };
        let Some(cell) = project.manager.get(uri) else {
            return Ok(());
        };
        let doc = cell.borrow();
        let mut diags = parse_diagnostics(doc.root_node(), &doc.text);
        if let Some(module) = project.analysis.module(uri) {
            diags.extend(diagnostics_from_module(&doc.text, module, self.lint_libs));
        }
        // P15.5 — pragma resolution diagnostics. Recomputed on every
        // publish so edits to `@include` / `@library` pragmas reflect
        // immediately. Anchored to the owning project's root.
        // Skipped for the implicit empty-root project (P32.1 lazy
        // fallback) so we don't anchor paths against a meaningless
        // cwd.
        if !project.root.as_os_str().is_empty() {
            let desc = parse_module_desc(uri.clone(), &doc.text, doc.root_node());
            if let Ok(ctx) = FsContext::new() {
                diags.extend(pragma_diagnostics(&doc.text, &desc, &project.root, &ctx));
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
    // P32.1 — per workspace folder, build a fresh `Project` rather
    // than overwriting a single global one.
    /// Resolve a workspace-folder URI to a local path, look for
    /// `project.gcl` at its root, and recursively load every reachable
    /// module via [`SourceManager::load_project`] into a fresh
    /// [`Project`]. Errors are logged but don't fail the LSP handshake.
    fn load_workspace(&mut self, ws_uri: &Uri) {
        let Some(ws_root) = uri_to_path(ws_uri) else {
            warn!("skipping non-file workspace folder: {}", ws_uri.as_str());
            return;
        };
        // Remember the workspace folder regardless of whether it has a
        // project.gcl — P32.3's parent walk needs to know its bounds
        // even when nothing was loaded eagerly.
        if !self.workspace_roots.contains(&ws_root) {
            self.workspace_roots.push(ws_root.clone());
        }
        let project_file = ws_root.join("project.gcl");
        if !project_file.is_file() {
            debug!(
                "no project.gcl in workspace {} — skipping recursive load",
                ws_root.display()
            );
            return;
        }
        let project_root = ws_root.clone();
        let mut project = Project::new(project_root.clone());
        let load_start = Instant::now();
        let report = project.manager.load_project(&project_file);
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
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[load_project] {} files in {load_took:?} (parse+load) + {rebuild_took:?} (analyze) from {}",
            report.loaded.len(),
            project_file.display()
        );
        let loaded = report.loaded.clone();
        for uri in &report.reachable {
            self.uri_owner.insert(uri.clone(), project_root.clone());
        }
        self.projects.insert(project_root, project);
        for (uri, _) in loaded {
            if let Err(e) = self.publish_for(&uri) {
                warn!("publish_for({}) failed: {e}", uri.as_str());
            }
        }
    }

    pub fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let doc = Document::new(params.text_document);
        debug!("[did_open] {doc}");
        // P32.3 — route via parent-walk owner search if the URI isn't
        // already bound to a project. Found owner not yet loaded → spin
        // up a fresh Project lazily. No owner → fall through to the
        // implicit empty-root project that P32.5 will replace with
        // dedicated orphan handling.
        let target_root = self.resolve_owner_for_did_open(&uri);
        if let Some(project) = self.projects.get_mut(&target_root) {
            project.manager.add(doc);
        }
        self.invalidate_with_slow_warning(&uri, "did_open");
        self.publish_for(&uri)?;
        Ok(())
    }

    // P32.3
    /// Pick the project that should own `uri` and ensure it's loaded.
    ///
    /// Returns the project-root key, ready to use against
    /// `self.projects`. The URI is bound in `uri_owner` so subsequent
    /// requests dispatch in O(1) without re-walking.
    fn resolve_owner_for_did_open(&mut self, uri: &Uri) -> PathBuf {
        if let Some(root) = self.uri_owner.get(uri).cloned() {
            return root;
        }
        let owner = find_owning_project_root(uri, &self.workspace_roots);
        let target = match owner {
            Some(root) => {
                if !self.projects.contains_key(&root) {
                    self.spawn_lazy_project(&root);
                }
                root
            }
            None => {
                // Orphan: P32.5 will route here with dedicated
                // handling. For now route to the implicit empty-root
                // project so behaviour matches today's single-project
                // analysis.
                let r = PathBuf::new();
                self.projects
                    .entry(r.clone())
                    .or_insert_with(|| Project::new(r.clone()));
                r
            }
        };
        self.bind_uri(uri.clone(), target.clone());
        target
    }

    // P32.3
    /// Spawn a fresh [`Project`] rooted at `root`, load its
    /// `@library` / `@include` closure, run a project-wide analyze
    /// pass, and index every reachable URI under that project. Mirrors
    /// the eager [`load_workspace`] path but for projects discovered
    /// on-demand by the parent walk.
    fn spawn_lazy_project(&mut self, root: &Path) {
        let project_file = root.join("project.gcl");
        let mut project = Project::new(root.to_path_buf());
        let load_start = Instant::now();
        let report = project.manager.load_project(&project_file);
        let load_took = load_start.elapsed();
        for lib in &report.unresolved_libraries {
            warn!("unresolved @library('{lib}') in {}", project_file.display());
        }
        for err in &report.errors {
            warn!("[lazy-load] {err}");
        }
        let rebuild_start = Instant::now();
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[lazy-load] {} files in {load_took:?} (parse+load) + {rebuild_took:?} (analyze) from {}",
            report.loaded.len(),
            project_file.display()
        );
        let root_path = root.to_path_buf();
        for u in &report.reachable {
            self.uri_owner.insert(u.clone(), root_path.clone());
        }
        self.projects.insert(root_path, project);
    }

    pub fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let Some(project) = self.project_for_mut(&uri) else {
            debug!(
                "[did_change] {} has no owning project — dropping",
                uri.as_str()
            );
            return Ok(());
        };
        let doc =
            project
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
            self.reload_project_closure(&uri);
        } else {
            self.invalidate_with_slow_warning(&uri, "did_change");
        }
        self.publish_for(&uri)?;
        Ok(())
    }

    /// True when `uri` resolves to its owning project's `project.gcl`.
    /// Used to gate the re-walk on `did_change` and to dispatch
    /// `lib/installed` / `*.gcl` events from the workspace watcher.
    fn is_project_entrypoint(&self, uri: &Uri) -> bool {
        let Some(project) = self.project_for(uri) else {
            return false;
        };
        let entrypoint = project.root.join("project.gcl");
        let Ok(canonical) = entrypoint.canonicalize() else {
            return false;
        };
        let Some(path) = uri_to_path(uri) else {
            return false;
        };
        path.canonicalize().map(|p| p == canonical).unwrap_or(false)
    }

    /// Re-walk a project's `@library` / `@include` closure against the
    /// current in-memory `project.gcl` and rebuild its analysis.
    /// Idempotent: `SourceManager::load_file` skips already-loaded
    /// files (including the in-editor entrypoint), so only newly-
    /// referenced modules get parsed. Triggered from `did_change` on
    /// the entrypoint and from the `lib/installed` branch of the
    /// workspace watcher. `trigger_uri` is the URI whose change
    /// prompted the reload — used only to pick the project to reload.
    fn reload_project_closure(&mut self, trigger_uri: &Uri) {
        let Some(project_root) = self.uri_owner.get(trigger_uri).cloned() else {
            return;
        };
        self.reload_project_closure_for(&project_root);
    }

    fn reload_project_closure_for(&mut self, project_root: &Path) {
        let Some(project) = self.projects.get_mut(project_root) else {
            return;
        };
        let project_file = project.root.join("project.gcl");
        let load_start = Instant::now();
        let report = project.manager.load_project(&project_file);
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
        let reachable: FxHashSet<Uri> = report.reachable.iter().cloned().collect();
        let evicted = project.manager.evict_unreachable(&reachable);
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
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[reload_project] {load_took:?} (parse+load) + {rebuild_took:?} (analyze) — closure: {} reachable, {} added, {} evicted",
            report.reachable.len(),
            report.loaded.len(),
            evicted.len(),
        );
        // Snapshot the report fields we'll need after we drop the
        // mutable borrow on `self.projects` so we can update
        // `uri_owner` and publish without overlapping borrows.
        let reachable_uris = report.reachable.clone();
        let owner_root = project_root.to_path_buf();

        // Re-index `uri_owner` for everything still reachable, and
        // drop entries for the evicted URIs.
        for uri in &evicted {
            self.uri_owner.remove(uri);
        }
        for uri in &reachable_uris {
            self.uri_owner.insert(uri.clone(), owner_root.clone());
        }

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
        for uri in &reachable_uris {
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
        let Some(project) = self.project_for_mut(uri) else {
            return;
        };
        let start = Instant::now();
        project.analysis.invalidate(&project.manager, uri);
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

    // P19.22 / P32.1
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
    /// — they go through the textDocument flow. P32.7 will replace the
    /// "reload every project" fan-out below with per-project routing
    /// based on which project owns each event's path.
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
                .project_for(uri)
                .and_then(|p| p.manager.get(uri))
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
                    if let Ok(text) = std::fs::read_to_string(&path)
                        && let Some(project) = self.project_for_mut(uri)
                    {
                        project
                            .manager
                            .add_simple(uri.clone(), text, "project", false);
                        self.invalidate_with_slow_warning(uri, "watch");
                        if let Err(e) = self.publish_for(uri) {
                            warn!("publish_for({}) failed: {e}", uri.as_str());
                        }
                    }
                }
                FileChangeType::DELETED => {
                    debug!("[watch] deleted {}", uri.as_str());
                    if let Some(project) = self.project_for_mut(uri) {
                        let _ = project.manager.remove(uri);
                    }
                    self.uri_owner.remove(uri);
                    self.publish_diagnostics(uri.clone(), Vec::new(), None)?;
                    needs_reload = true;
                }
                _ => {}
            }
        }
        if needs_reload {
            // P32.7 will narrow this to the projects whose closures
            // actually changed. For now, fan out to every loaded
            // project so we don't silently miss reloads in a
            // multi-project workspace.
            let roots: Vec<PathBuf> = self.projects.keys().cloned().collect();
            for root in roots {
                self.reload_project_closure_for(&root);
            }
        }
        Ok(())
    }
}

/// Convert a `file://` URI to a local path. Returns `None` for non-file
/// schemes (LSP technically allows `untitled:`, `git:`, etc.).
pub(crate) fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(Path::new(stripped).to_path_buf())
}

// P32.3
/// Walk up from `uri`'s directory looking for the nearest `project.gcl`,
/// bounded by the enclosing workspace folder.
///
/// Returns the directory holding the closest `project.gcl`, or `None`
/// when the URI is outside every workspace folder or the walk reaches
/// the workspace folder root without finding one.
///
/// The walk *includes* the workspace folder itself — if a project.gcl
/// sits exactly at the workspace folder root, that wins.
pub(crate) fn find_owning_project_root(uri: &Uri, workspace_roots: &[PathBuf]) -> Option<PathBuf> {
    let path = uri_to_path(uri)?;
    let start_dir = path.parent()?.to_path_buf();
    // Find the workspace folder that contains the URI. Multiple
    // matches: pick the longest (most specific) one, since nested
    // workspace folders are valid in LSP.
    let ws_root = workspace_roots
        .iter()
        .filter(|ws| start_dir.starts_with(ws))
        .max_by_key(|ws| ws.as_os_str().len())?
        .clone();
    let mut cur = start_dir;
    loop {
        if cur.join("project.gcl").is_file() {
            return Some(cur);
        }
        if cur == ws_root {
            return None;
        }
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => return None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Receiver, unbounded};
    use std::str::FromStr;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).unwrap()
    }

    /// Test backend bundled with the receiving end of its publish
    /// channel — the receiver MUST outlive every `publish_*` call,
    /// otherwise `Sender::send` errors out.
    fn backend() -> (Backend, Receiver<Message>) {
        let (tx, rx) = unbounded();
        (
            Backend {
                client: tx,
                projects: FxHashMap::default(),
                uri_owner: FxHashMap::default(),
                workspace_roots: Vec::new(),
                lint_libs: false,
                registry: None,
            },
            rx,
        )
    }

    // P32.1
    /// `project_for` routes by `uri_owner`. Two projects coexist;
    /// each URI dispatches to its bound root and never bleeds across.
    #[test]
    fn project_for_routes_by_uri_owner() {
        let (mut b, _rx) = backend();
        let root_a = PathBuf::from("/ws/projA");
        let root_b = PathBuf::from("/ws/projB");
        b.projects
            .insert(root_a.clone(), Project::new(root_a.clone()));
        b.projects
            .insert(root_b.clone(), Project::new(root_b.clone()));

        let a1 = uri("file:///ws/projA/main.gcl");
        let b1 = uri("file:///ws/projB/main.gcl");
        b.uri_owner.insert(a1.clone(), root_a.clone());
        b.uri_owner.insert(b1.clone(), root_b.clone());

        assert_eq!(b.project_for(&a1).map(|p| &p.root), Some(&root_a));
        assert_eq!(b.project_for(&b1).map(|p| &p.root), Some(&root_b));

        // Unowned URI: no routing.
        let unbound = uri("file:///elsewhere/loose.gcl");
        assert!(b.project_for(&unbound).is_none());
    }

    // P32.3
    /// Set up a temp directory with the nested layout
    /// `outer/project.gcl` + `outer/sub/project.gcl` and return the
    /// outer / inner roots plus the temp base so the caller can clean
    /// up.
    fn fixture_nested_projects(slug: &str) -> (PathBuf, PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "gca_owner_search_{}_{}_{}",
            slug,
            std::process::id(),
            // Cheap monotonically-increasing-ish suffix so parallel
            // tests don't collide on the same dir.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let outer = tmp.join("outer");
        let inner = outer.join("sub");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(
            outer.join("project.gcl"),
            "fn outer_root(): int { return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            inner.join("project.gcl"),
            "fn inner_root(): int { return 0; }\n",
        )
        .unwrap();
        (tmp, outer, inner)
    }

    fn path_uri(p: &Path) -> Uri {
        Uri::from_str(&format!("file://{}", p.display())).unwrap()
    }

    // P32.3
    /// Pure-function test of the parent-walk: nearest `project.gcl`
    /// wins, walk bounded by the workspace folder, files outside
    /// every workspace folder return `None`.
    #[test]
    fn find_owning_project_root_picks_nearest() {
        let (tmp, outer, inner) = fixture_nested_projects("nearest");
        let workspace_roots = vec![outer.clone()];

        // File in inner dir → inner wins.
        let foo_uri = path_uri(&inner.join("foo.gcl"));
        assert_eq!(
            find_owning_project_root(&foo_uri, &workspace_roots),
            Some(inner.clone())
        );

        // File in outer dir (sibling of `sub`) → outer wins (inner is
        // not on the walk path).
        let bar_uri = path_uri(&outer.join("bar.gcl"));
        assert_eq!(
            find_owning_project_root(&bar_uri, &workspace_roots),
            Some(outer.clone())
        );

        // File outside every workspace folder → no owner.
        let elsewhere_uri = path_uri(&tmp.join("elsewhere.gcl"));
        assert_eq!(
            find_owning_project_root(&elsewhere_uri, &workspace_roots),
            None
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // P32.3
    /// `did_open` for a file in the eagerly-loaded outer project keeps
    /// using outer; `did_open` for a file under the inner project
    /// spawns it lazily.
    #[test]
    fn did_open_routes_to_nearest_project() {
        let (tmp, outer, inner) = fixture_nested_projects("did_open");
        let (mut b, _rx) = backend();
        b.workspace_roots.push(outer.clone());

        // Simulate eager load of outer.
        let outer_uri = path_uri(&outer);
        b.load_workspace(&outer_uri);
        assert!(
            b.projects.contains_key(&outer),
            "outer project should be loaded eagerly"
        );
        assert!(
            !b.projects.contains_key(&inner),
            "inner project must not be loaded eagerly"
        );

        // Open outer/bar.gcl — owner is outer (already loaded).
        let bar_uri = path_uri(&outer.join("bar.gcl"));
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: bar_uri.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "fn bar(): int { return 0; }\n".into(),
            },
        })
        .unwrap();
        assert_eq!(b.uri_owner.get(&bar_uri), Some(&outer));
        assert!(
            !b.projects.contains_key(&inner),
            "opening outer/bar.gcl must not spawn inner project"
        );

        // Open outer/sub/foo.gcl — owner is inner, must spawn lazily.
        let foo_uri = path_uri(&inner.join("foo.gcl"));
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: foo_uri.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "fn foo(): int { return 0; }\n".into(),
            },
        })
        .unwrap();
        assert_eq!(b.uri_owner.get(&foo_uri), Some(&inner));
        assert!(
            b.projects.contains_key(&inner),
            "opening outer/sub/foo.gcl must spawn inner project"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
