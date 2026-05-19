use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::module_desc::parse_module_desc;
use greycat_analyzer_core::registry::RegistryFetcher;
use greycat_analyzer_core::resolver::FsContext;
use greycat_analyzer_core::{
    Document, SourceManager, StdResolution,
    diagnostics::{
        duplicate_module_name_diagnostic, missing_std_diagnostic, multi_project_owner_diagnostic,
        orphan_module_diagnostic, parse_diagnostics, pragma_diagnostics,
    },
};
use log::{debug, info, warn};
use lsp_server::*;
use lsp_types::{
    notification::{Notification as _, PublishDiagnostics},
    request::{RegisterCapability, Request as _},
    *,
};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

// P32.6
/// Per-URI owner list. Most files have exactly one owner; rare
/// overlap between two projects' closures pushes a second entry
/// onto the inline tail.
pub type OwnerList = SmallVec<[PathBuf; 1]>;

use crate::Result;
use crate::capabilities::diagnostics_from_module;

/// Threshold above which a per-edit rebuild is surfaced as a one-line
/// `info!` so users see hot spots without flooding the log on healthy
/// edits. Tuned by hand — typical stdlib rebuilds land at ~30-100ms,
/// so 500ms catches genuine outliers.
const SLOW_REBUILD_MS: u128 = 500;

/// Default leading-edge + trailing debounce window for analyzer-side
/// diagnostic publishes. Within this window after the last analyzer
/// publish, `did_change` only emits a fast (parse + overlays + cached
/// analyzer) publish and schedules a trailing analyzer fire instead of
/// re-running the analyzer on every keystroke. Sized to sit below the
/// "feels delayed" threshold (~200ms) and above typical inter-keystroke
/// times at fast typing (~80-100ms) so bursts coalesce reliably.
///
/// Override per-client via `initializationOptions.diagnosticsDebounceMs`.
pub const DIAGNOSTICS_DEBOUNCE_DEFAULT: Duration = Duration::from_millis(150);

// P32.9
/// Static log tag for orphan-file handling. Appears as `[orphan]`
/// in front of any log line emitted from the orphan path.
pub const ORPHAN_LOG_TAG: &str = "orphan";
// P32.9
/// Static log tag for server lifecycle (`initialized`, capability
/// registration, workspace-folder add/remove). Appears as `[server]`.
pub const SERVER_LOG_TAG: &str = "server";
// P32.9
/// Tag for the implicit empty-root project that catches headless
/// single-file LSP sessions (no `workspace_folders` at all).
pub const SINGLE_FILE_LOG_TAG: &str = "single-file";

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
    // P32.9
    /// Pre-computed log tag — the project root's path relative to
    /// its enclosing workspace folder, or the workspace folder's
    /// basename when the project lives at the folder root.
    pub tag: String,
    // P33.1
    /// Where the resolver found `std`. Drives the `missing-std`
    /// advisory in [`Backend::publish_for`].
    pub std_resolution: StdResolution,
    // P33.1
    /// URI of the project's `project.gcl` entrypoint, captured from
    /// the loader's `LoadReport` so we can compare against incoming
    /// publishes without re-canonicalizing.
    pub entrypoint_uri: Option<Uri>,
}

impl Project {
    pub fn new(root: PathBuf, tag: String) -> Self {
        Self {
            root,
            manager: SourceManager::new(),
            analysis: ProjectAnalysis::new(),
            tag,
            std_resolution: StdResolution::default(),
            entrypoint_uri: None,
        }
    }
}

// P32.9
/// Compute the `[<tag>]` log prefix for a project rooted at `root`.
///
/// Resolution order:
/// 1. Find the longest workspace folder that contains `root`.
/// 2. `root` relative to that folder — that's the tag.
/// 3. If the relative path is empty (root == workspace folder), fall
///    back to the workspace folder's basename.
/// 4. If `root` isn't under any workspace folder (lazy spawn for a
///    file dropped outside, or empty `workspace_roots`), use the
///    root's basename. If even that is empty (`PathBuf::new()` —
///    the headless single-file project), use [`SINGLE_FILE_LOG_TAG`].
pub(crate) fn compute_project_tag(root: &Path, workspace_roots: &[PathBuf]) -> String {
    if root.as_os_str().is_empty() {
        return SINGLE_FILE_LOG_TAG.to_string();
    }
    if let Some(ws) = workspace_roots
        .iter()
        .filter(|w| root.starts_with(w))
        .max_by_key(|w| w.as_os_str().len())
    {
        let rel = root.strip_prefix(ws).unwrap_or(root);
        if rel.as_os_str().is_empty() {
            return ws
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("project")
                .to_string();
        }
        return rel.display().to_string();
    }
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string()
}

// P32.5
/// Routing decision for an opened document.
enum DocOwner {
    /// File belongs to the project rooted at this directory key.
    Project(PathBuf),
    /// File is in `self.orphans` — parse-only, no analysis.
    Orphan,
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
    /// URI → owning-project roots. Populated eagerly during
    /// `load_workspace` and on `did_open` for files that aren't part
    /// of any project's closure. Almost always a single-element
    /// [`OwnerList`]; when two projects' `@include` closures reach
    /// the same file (a design error — surfaced via the
    /// `multi-project-owner` advisory in [`publish_for`]) the list
    /// grows past one entry.
    pub uri_owner: FxHashMap<Uri, OwnerList>,
    // P32.5
    /// Documents opened from outside every workspace folder's
    /// `project.gcl` closure. Parsed for syntax diagnostics but not
    /// fed through any project's analysis pipeline. Each gets a
    /// file-spanning [`orphan_module_diagnostic`] so the editor dims
    /// it and explains why. Stored in a `SourceManager` for the
    /// `update` API; the manager's `load_project` is never called.
    pub orphans: SourceManager,
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
    /// Negociated encoding between the client and the server
    pub encoding: SourceEncoding,
    /// Per-URI timestamp of the last analyzer-side diagnostic publish.
    /// Drives the leading-edge gate in [`Self::did_change`]: a
    /// `did_change` whose timestamp is `>= DIAGNOSTICS_DEBOUNCE` past
    /// this entry fires the analyzer immediately; one within the
    /// window emits only a fast publish and schedules a trailing fire.
    pub last_analyzer_publish: FxHashMap<Uri, Instant>,
    /// Per-URI deadline at which a trailing analyzer publish should
    /// fire. Set inside the cool-down window on `did_change`; cleared
    /// by [`Self::flush_trailing`] after the publish.
    pub pending_trailing: FxHashMap<Uri, Instant>,
    /// Per-URI cache of the most recent analyzer-derived diagnostic set
    /// (i.e. just the [`diagnostics_from_module`] output, *without* the
    /// parse / pragma / overlay diagnostics). Re-merged with fresh
    /// fast-side diagnostics on every cool-down-window `did_change` so
    /// the editor keeps showing semantic squiggles stably while the
    /// user types instead of flickering.
    pub last_analyzer_diags: FxHashMap<Uri, Vec<Diagnostic>>,
    /// Leading-edge + trailing debounce window for analyzer publishes.
    /// Defaults to [`DIAGNOSTICS_DEBOUNCE_DEFAULT`]; client-overridable
    /// via `initializationOptions.diagnosticsDebounceMs` (parsed in
    /// [`Self::initialized`]). A value of `0` effectively disables the
    /// debounce — every keystroke fires the analyzer immediately
    /// (matches pre-debounce behavior).
    pub diagnostics_debounce: Duration,
}

impl Backend {
    // P32.1
    /// Look up the [`Project`] that owns `uri`. Returns the first
    /// (primary) owner when several exist — multi-owner conflicts
    /// surface as a separate [`multi_project_owner_diagnostic`] via
    /// [`publish_for`], not by altering routing.
    pub fn project_for(&self, uri: &Uri) -> Option<&Project> {
        let root = self.uri_owner.get(uri)?.first()?;
        self.projects.get(root)
    }

    pub fn project_for_mut(&mut self, uri: &Uri) -> Option<&mut Project> {
        let root = self.uri_owner.get(uri)?.first()?.clone();
        self.projects.get_mut(&root)
    }

    // P32.6
    /// Append `root` to `uri`'s owner list. Idempotent: a project
    /// re-binding a URI it already owns is a no-op.
    fn bind_uri(&mut self, uri: Uri, root: PathBuf) {
        let owners = self.uri_owner.entry(uri).or_default();
        if !owners.iter().any(|p| p == &root) {
            owners.push(root);
        }
    }

    // P32.9
    /// Comma-separated list of project tags for the given owner
    /// roots. Used to label watcher / multi-owner log lines.
    fn owners_tag_csv(&self, owners: &[PathBuf]) -> String {
        let tags: Vec<&str> = owners
            .iter()
            .filter_map(|r| self.projects.get(r).map(|p| p.tag.as_str()))
            .collect();
        if tags.is_empty() {
            SERVER_LOG_TAG.into()
        } else {
            tags.join(",")
        }
    }

    // P32.6
    /// Drop `root` from `uri`'s owner list; remove the entry entirely
    /// when no owners remain. Used by reload-time eviction so a file
    /// only leaving project A's closure (while still in B's) ends up
    /// with the right shrunk list.
    fn unbind_uri(&mut self, uri: &Uri, root: &Path) {
        if let Some(owners) = self.uri_owner.get_mut(uri) {
            owners.retain(|p| p != root);
            if owners.is_empty() {
                self.uri_owner.remove(uri);
            }
        }
    }

    fn publish_diagnostics(
        &self,
        uri: Uri,
        diagnostics: Vec<Diagnostic>,
        version: Option<i32>,
    ) -> Result<()> {
        publish_diagnostics(&self.client, uri, diagnostics, version)
    }

    /// Assemble the cheap "fast" diagnostic set + the heavier analyzer
    /// set + the doc's version, all from cached state. Splits the old
    /// monolithic `publish_for` body so a `did_change` mid-burst can
    /// emit only the fast half (parse + overlays + cached analyzer
    /// from a previous publish) without re-running the analyzer.
    ///
    /// Fast set: parse + pragma resolution + multi-owner / missing-std
    /// / duplicate-module overlays. All cheap; the parse tree is in
    /// hand from `manager.update`.
    /// Analyzer set: `diagnostics_from_module` output only.
    fn assemble_split(&self, uri: &Uri) -> (Vec<Diagnostic>, Vec<Diagnostic>, Option<i32>) {
        let Some(project) = self.project_for(uri) else {
            return (Vec::new(), Vec::new(), None);
        };
        let Some(cell) = project.manager.get(uri) else {
            return (Vec::new(), Vec::new(), None);
        };
        let doc = cell.borrow();
        let version = Some(doc.version);
        let mut fast = parse_diagnostics(doc.root_node(), &doc.text);
        // P15.5 — pragma resolution diagnostics. Recomputed on every
        // publish so edits to `@include` / `@library` pragmas reflect
        // immediately. Anchored to the owning project's root. Skipped
        // for the implicit empty-root project (P32.1 lazy fallback).
        if !project.root.as_os_str().is_empty() {
            let desc = parse_module_desc(uri.clone(), &doc.text, doc.root_node());
            if let Ok(ctx) = FsContext::new() {
                fast.extend(pragma_diagnostics(&doc.text, &desc, &project.root, &ctx));
            }
        }
        // P32.6 — multi-project-owner advisory.
        if let Some(owners) = self.uri_owner.get(uri)
            && owners.len() > 1
        {
            fast.push(multi_project_owner_diagnostic(&doc.text, owners));
        }
        // P33.1 — `missing-std` overlay on the entrypoint when the
        // resolver couldn't find std.
        if project.std_resolution == StdResolution::Missing
            && project.entrypoint_uri.as_ref() == Some(uri)
        {
            fast.push(missing_std_diagnostic(&doc.text));
        }
        // `duplicate-module-name` overlay on stem-colliding files.
        if let Some((name, existing)) = project.analysis.index.duplicate_modules.get(uri) {
            let module_name = &project.analysis.index.symbols[*name];
            fast.push(duplicate_module_name_diagnostic(
                &doc.text,
                module_name,
                existing,
            ));
        }
        let analyzer = project
            .analysis
            .module(uri)
            .map(|m| diagnostics_from_module(&doc.text, m, self.lint_libs))
            .unwrap_or_default();
        (fast, analyzer, version)
    }

    /// Pull cached parse + semantic + lint diagnostics for `uri` and
    /// push them to the client. The cache is populated by
    /// [`ProjectAnalysis::analyze`] on workspace load and by
    /// [`ProjectAnalysis::invalidate`] on every `did_open` / `did_change`.
    /// Also seeds [`Self::last_analyzer_diags`] and
    /// [`Self::last_analyzer_publish`] / clears any pending trailing
    /// fire so the debounce treats this as a fresh leading edge.
    fn publish_for(&mut self, uri: &Uri) -> Result<()> {
        // P32.5 — orphans take a separate parse-only path with a
        // file-spanning "no project" advisory diag. No analyzer state
        // to cache here, but clearing any stale cache entry keeps the
        // maps from leaking across project ↔ orphan transitions.
        if let Some(cell) = self.orphans.get(uri) {
            let doc = cell.borrow();
            let mut diags = parse_diagnostics(doc.root_node(), &doc.text);
            diags.push(orphan_module_diagnostic(&doc.text));
            let version = doc.version;
            drop(doc);
            self.last_analyzer_publish.remove(uri);
            self.last_analyzer_diags.remove(uri);
            self.pending_trailing.remove(uri);
            return self.publish_diagnostics(uri.clone(), diags, Some(version));
        }
        let (fast, analyzer, version) = self.assemble_split(uri);
        if version.is_none() {
            return Ok(());
        }
        let mut combined = fast;
        combined.extend(analyzer.iter().cloned());
        self.last_analyzer_diags.insert(uri.clone(), analyzer);
        self.last_analyzer_publish
            .insert(uri.clone(), Instant::now());
        self.pending_trailing.remove(uri);
        self.publish_diagnostics(uri.clone(), combined, version)
    }

    /// Cool-down-window publish: emit fresh fast diagnostics merged
    /// with the cached analyzer set. Cheap — no `invalidate`, no
    /// fresh `diagnostics_from_module` call. Used by `did_change` for
    /// the second-through-N edits of a typing burst so semantic
    /// squiggles stay stable while syntax errors update immediately.
    fn publish_fast(&self, uri: &Uri) -> Result<()> {
        let (mut combined, _analyzer, version) = self.assemble_split(uri);
        if version.is_none() {
            return Ok(());
        }
        if let Some(cached) = self.last_analyzer_diags.get(uri) {
            combined.extend_from_slice(cached);
        }
        self.publish_diagnostics(uri.clone(), combined, version)
    }

    /// Schedule a trailing analyzer publish for `uri` if one isn't
    /// already pending. Anchored to the last leading-edge publish +
    /// `debounce` so the trailing fires at a stable deadline
    /// regardless of how many keystrokes arrive during the burst —
    /// without this anchor, every keystroke would push the deadline
    /// further out and the trailing would never fire on a continuous
    /// typist.
    fn schedule_trailing(&mut self, uri: Uri, debounce: Duration) {
        if self.pending_trailing.contains_key(&uri) {
            return;
        }
        let deadline = self
            .last_analyzer_publish
            .get(&uri)
            .map(|t| *t + debounce)
            .unwrap_or_else(|| Instant::now() + debounce);
        self.pending_trailing.insert(uri, deadline);
    }

    /// Earliest pending trailing deadline across all URIs, used by
    /// the main loop's `select!` timeout-arm. `None` means no trailing
    /// is scheduled — the main loop falls back to a long idle timer.
    pub fn next_trailing_deadline(&self) -> Option<Instant> {
        self.pending_trailing.values().min().copied()
    }

    /// Drain every trailing entry whose deadline has passed and run
    /// the analyzer-side publish for each. Each fire re-uses
    /// [`Self::invalidate_with_slow_warning`] + [`Self::publish_for`]
    /// so the trailing path is identical to a leading-edge fire (just
    /// time-shifted by the debounce window).
    pub fn flush_trailing(&mut self) {
        let now = Instant::now();
        let due: Vec<Uri> = self
            .pending_trailing
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(u, _)| u.clone())
            .collect();
        for uri in due {
            self.pending_trailing.remove(&uri);
            self.invalidate_with_slow_warning(&uri, "trailing");
            if let Err(e) = self.publish_for(&uri) {
                warn!(
                    "[{tag}][trailing] publish failed for {}: {e}",
                    uri.as_str(),
                    tag = self.tag_for(&uri)
                );
            }
        }
    }

    pub fn initialized(&mut self, init: &InitializeParams) -> Result<()> {
        // Pull `lintLibs` (and any future settings) out of
        // `initializationOptions` before walking workspace folders so
        // the very first round of `publish_for` already honors the
        // user's preference. Missing / malformed payload silently
        // falls back to the default (`false`) — no need to fail the
        // handshake over an absent option.
        if let Some(opts) = init.initialization_options.as_ref() {
            if let Some(b) = opts.get("lintLibs").and_then(|v| v.as_bool()) {
                self.lint_libs = b;
                debug!("[{SERVER_LOG_TAG}][init] lintLibs={b}");
            }
            if let Some(ms) = opts.get("diagnosticsDebounceMs").and_then(|v| v.as_u64()) {
                self.diagnostics_debounce = Duration::from_millis(ms);
                debug!("[{SERVER_LOG_TAG}][init] diagnosticsDebounceMs={ms}");
            }
        }
        if let Some(workspaces) = init.workspace_folders.as_ref() {
            debug!("[{SERVER_LOG_TAG}] workspaces:");
            for ws in workspaces {
                debug!("[{SERVER_LOG_TAG}] - {}={}", ws.name, ws.uri.as_str());
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
            debug!(
                "[{SERVER_LOG_TAG}][init] client does not support didChangeWatchedFiles dynamic registration"
            );
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
        let tag = compute_project_tag(&project_root, &self.workspace_roots);
        let mut project = Project::new(project_root.clone(), tag.clone());
        let load_start = Instant::now();
        let report = project.manager.load_project(&project_file);
        let load_took = load_start.elapsed();
        project.std_resolution = report.std_resolution;
        project.entrypoint_uri = report.entrypoint_uri.clone();
        if report.std_resolution == StdResolution::Missing {
            warn!("[{tag}][load_project] std not found (local or $HOME/.greycat)");
        }
        for lib in &report.unresolved_libraries {
            warn!(
                "[{tag}] unresolved @library('{lib}') in {}",
                project_file.display()
            );
        }
        for err in &report.errors {
            warn!("[{tag}][load_project] {err}");
        }
        // Single project-wide pipeline pass over everything we just
        // loaded — the per-doc analyses land in the cache.
        let rebuild_start = Instant::now();
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[{tag}][load_project] {} files in {load_took:?} (parse+load) + {rebuild_took:?} (analyze) from {}",
            report.loaded.len(),
            project_file.display()
        );
        let loaded = report.loaded.clone();
        let reachable = report.reachable.clone();
        self.projects.insert(project_root.clone(), project);
        for uri in &reachable {
            self.bind_uri(uri.clone(), project_root.clone());
        }
        for (uri, _) in loaded {
            if let Err(e) = self.publish_for(&uri) {
                warn!("publish_for({}) failed: {e}", uri.as_str());
            }
        }
    }

    pub fn did_open(&mut self, params: DidOpenTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        let doc = Document::new(params.text_document);
        // P32.5 — three-way routing:
        //
        // - URI already bound to a project (eager-load or prior
        //   did_open) → reuse that project.
        // - URI walks to a `project.gcl` → spawn lazily if needed.
        // - No workspace folder match, but `workspace_roots` IS empty
        //   (headless single-file LSP session) → implicit empty-root
        //   project gets full analysis (preserves the smoke-test path).
        // - No workspace folder match, but `workspace_roots` is
        //   populated → orphan: parse-only, dim diag, no analysis.
        match self.resolve_owner_for_did_open(&uri) {
            DocOwner::Project(root) => {
                if let Some(project) = self.projects.get_mut(&root) {
                    debug!("[{}][did_open] {doc}", project.tag);
                    project.manager.add(doc);
                }
                self.invalidate_with_slow_warning(&uri, "did_open");
            }
            DocOwner::Orphan => {
                debug!("[{ORPHAN_LOG_TAG}][did_open] {doc}");
                self.orphans.add(doc);
            }
        }
        self.publish_for(&uri)?;
        Ok(())
    }

    // P32.3 / P32.5
    /// Decide who owns `uri` and (for project files) ensure that
    /// project is loaded.
    fn resolve_owner_for_did_open(&mut self, uri: &Uri) -> DocOwner {
        if let Some(root) = self.uri_owner.get(uri).and_then(|o| o.first()).cloned() {
            return DocOwner::Project(root);
        }
        if self.orphans.get(uri).is_some() {
            return DocOwner::Orphan;
        }
        let owner = find_owning_project_root(uri, &self.workspace_roots);
        match owner {
            Some(root) => {
                if !self.projects.contains_key(&root) {
                    self.spawn_lazy_project(&root);
                }
                self.bind_uri(uri.clone(), root.clone());
                DocOwner::Project(root)
            }
            None => {
                if self.workspace_roots.is_empty() {
                    // Headless / single-file LSP session: keep the
                    // implicit empty-root project as the catch-all
                    // analysed bucket. Matches pre-P32 behaviour for
                    // clients that didn't supply `workspace_folders`.
                    let r = PathBuf::new();
                    self.projects
                        .entry(r.clone())
                        .or_insert_with(|| Project::new(r.clone(), SINGLE_FILE_LOG_TAG.into()));
                    self.bind_uri(uri.clone(), r.clone());
                    DocOwner::Project(r)
                } else {
                    // Inside a workspace but with no project.gcl up-tree
                    // — true orphan, no analysis.
                    DocOwner::Orphan
                }
            }
        }
    }

    // P32.3
    /// Spawn a fresh [`Project`] rooted at `root`, load its
    /// `@library` / `@include` closure, run a project-wide analyze
    /// pass, and index every reachable URI under that project. Mirrors
    /// the eager [`load_workspace`] path but for projects discovered
    /// on-demand by the parent walk.
    fn spawn_lazy_project(&mut self, root: &Path) {
        let project_file = root.join("project.gcl");
        let tag = compute_project_tag(root, &self.workspace_roots);
        let mut project = Project::new(root.to_path_buf(), tag.clone());
        let load_start = Instant::now();
        let report = project.manager.load_project(&project_file);
        let load_took = load_start.elapsed();
        project.std_resolution = report.std_resolution;
        project.entrypoint_uri = report.entrypoint_uri.clone();
        if report.std_resolution == StdResolution::Missing {
            warn!("[{tag}][lazy-load] std not found (local or $HOME/.greycat)");
        }
        for lib in &report.unresolved_libraries {
            warn!(
                "[{tag}] unresolved @library('{lib}') in {}",
                project_file.display()
            );
        }
        for err in &report.errors {
            warn!("[{tag}][lazy-load] {err}");
        }
        let rebuild_start = Instant::now();
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[{tag}][lazy-load] {} files in {load_took:?} (parse+load) + {rebuild_took:?} (analyze) from {}",
            report.loaded.len(),
            project_file.display()
        );
        let root_path = root.to_path_buf();
        let reachable = report.reachable.clone();
        self.projects.insert(root_path.clone(), project);
        for u in &reachable {
            self.bind_uri(u.clone(), root_path.clone());
        }
    }

    pub fn did_change(&mut self, params: DidChangeTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri.clone();
        // P32.5 — orphans live in a separate SourceManager; updating
        // them is parse-only, no invalidate.
        if self.orphans.get(&uri).is_some() {
            debug!("[{ORPHAN_LOG_TAG}][did_change] {}", uri.as_str());
            let _ = self.orphans.update(
                &uri,
                params.content_changes,
                params.text_document.version,
                self.encoding,
            );
            self.publish_for(&uri)?;
            return Ok(());
        }
        let encoding = self.encoding;
        let Some(project) = self.project_for_mut(&uri) else {
            debug!(
                "[{SERVER_LOG_TAG}][did_change] {} has no owning project — dropping",
                uri.as_str()
            );
            return Ok(());
        };
        let doc = project.manager.update(
            &uri,
            params.content_changes,
            params.text_document.version,
            encoding,
        );
        debug!("[{}][did_change] {doc}", project.tag);
        drop(doc);
        // **P19.22** — pragma edits to the project entrypoint can add /
        // remove `@library` / `@include`, which changes the closure of
        // reachable modules. The text update above only refreshes
        // `project.gcl`'s own bytes; the resolver doesn't notice the
        // new modules until we re-walk. Entrypoint edits go through
        // the synchronous path (no debounce) because closure changes
        // must land before subsequent analyses run.
        if self.is_project_entrypoint(&uri) {
            self.reload_project_closure(&uri);
            return self.publish_for(&uri);
        }
        // Leading-edge + trailing debounce for non-entrypoint edits.
        // Within `DIAGNOSTICS_DEBOUNCE` of the last analyzer publish,
        // only emit a fast (parse + overlays + cached analyzer) publish
        // and schedule a trailing analyzer fire. Outside the window,
        // run the full invalidate + analyzer publish synchronously so
        // solo edits and the first edit of a burst feel instant.
        let now = Instant::now();
        let cooldown_expired = self
            .last_analyzer_publish
            .get(&uri)
            .map(|t| now.duration_since(*t) >= self.diagnostics_debounce)
            .unwrap_or(true);
        if cooldown_expired {
            self.invalidate_with_slow_warning(&uri, "did_change");
            self.publish_for(&uri)?;
        } else {
            self.publish_fast(&uri)?;
            let debounce = self.diagnostics_debounce;
            self.schedule_trailing(uri, debounce);
        }
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
        let Some(project_root) = self
            .uri_owner
            .get(trigger_uri)
            .and_then(|o| o.first())
            .cloned()
        else {
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
        project.std_resolution = report.std_resolution;
        project.entrypoint_uri = report.entrypoint_uri.clone();
        if report.std_resolution == StdResolution::Missing {
            warn!(
                "[{tag}][reload_project] std not found (local or $HOME/.greycat)",
                tag = project.tag
            );
        }
        for lib in &report.unresolved_libraries {
            warn!(
                "[{tag}] unresolved @library('{lib}') in {}",
                project_file.display(),
                tag = project.tag,
            );
        }
        for err in &report.errors {
            warn!("[{tag}][reload_project] {err}", tag = project.tag);
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
                "[{tag}][reload_project] evicted {} unreachable file(s):",
                evicted.len(),
                tag = project.tag
            );
            for uri in &evicted {
                info!(
                    "[{tag}][reload_project]   - {}",
                    uri.as_str(),
                    tag = project.tag
                );
            }
        }
        // Log additions explicitly too — symmetric with eviction so the
        // user can see project-closure changes in the LSP output.
        if !report.loaded.is_empty() {
            info!(
                "[{tag}][reload_project] loaded {} new file(s):",
                report.loaded.len(),
                tag = project.tag
            );
            for (uri, _) in &report.loaded {
                info!(
                    "[{tag}][reload_project]   + {}",
                    uri.as_str(),
                    tag = project.tag
                );
            }
        }
        let rebuild_start = Instant::now();
        project.analysis.rebuild(&project.manager);
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[{tag}][reload_project] {load_took:?} (parse+load) + {rebuild_took:?} (analyze) — closure: {} reachable, {} added, {} evicted",
            report.reachable.len(),
            report.loaded.len(),
            evicted.len(),
            tag = project.tag,
        );
        // Snapshot the report fields we'll need after we drop the
        // mutable borrow on `self.projects` so we can update
        // `uri_owner` and publish without overlapping borrows.
        let reachable_uris = report.reachable.clone();
        let owner_root = project_root.to_path_buf();

        // Re-index `uri_owner` for everything still reachable, and
        // drop *this project's* binding for the evicted URIs (other
        // projects' bindings on the same URI must survive — see P32.6).
        for uri in &evicted {
            self.unbind_uri(uri, &owner_root);
        }
        for uri in &reachable_uris {
            self.bind_uri(uri.clone(), owner_root.clone());
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
            info!(
                "[{tag}][slow-rebuild] {source} for {} took {took:?}",
                uri.as_str(),
                tag = project.tag,
            );
        }
    }

    pub fn did_save(&mut self, params: DidSaveTextDocumentParams) -> Result<()> {
        let uri = &params.text_document.uri;
        debug!(
            "[{tag}][did_save] {} (text={:?})",
            uri.as_str(),
            params.text,
            tag = self.tag_for(uri),
        );
        // Editors may format/lint on save and send the canonical text;
        // re-publish so any newly-introduced or newly-resolved errors
        // are visible.
        self.publish_for(uri)?;
        Ok(())
    }

    pub fn did_close(&mut self, params: DidCloseTextDocumentParams) -> Result<()> {
        let uri = params.text_document.uri;
        debug!(
            "[{tag}][did_close] {}",
            uri.as_str(),
            tag = self.tag_for(&uri)
        );
        // P32.5 — orphans don't outlive `did_close`. The dim diag
        // is purely an editor signal, so dropping the doc with the
        // buffer is correct.
        let _ = self.orphans.remove(&uri);
        // Drop the debounce caches so a later `did_open` of the same
        // URI starts from a clean slate (no stale leading-edge clock
        // or analyzer set).
        self.last_analyzer_publish.remove(&uri);
        self.last_analyzer_diags.remove(&uri);
        self.pending_trailing.remove(&uri);
        // Clear diagnostics on close so the editor's stale list goes away.
        self.publish_diagnostics(uri, Vec::new(), None)?;
        Ok(())
    }

    // P32.8
    /// LSP `workspace/didChangeWorkspaceFolders` notification.
    ///
    /// Added folder: rerun the eager discovery (mirrors the initial
    /// `initialized` handler) — push the folder onto `workspace_roots`,
    /// try to load `<folder>/project.gcl`.
    ///
    /// Removed folder: drop every project whose root is inside (or
    /// equal to) the removed folder. Clear diagnostics for every URI
    /// those projects owned, evict the URIs from `uri_owner`, and
    /// drop matching orphans.
    pub fn did_change_workspace_folders(
        &mut self,
        params: DidChangeWorkspaceFoldersParams,
    ) -> Result<()> {
        for ws in &params.event.added {
            let Some(path) = uri_to_path(&ws.uri) else {
                warn!(
                    "[{SERVER_LOG_TAG}][ws-folders] skipping non-file folder: {}",
                    ws.uri.as_str()
                );
                continue;
            };
            debug!(
                "[{SERVER_LOG_TAG}][ws-folders] + {} ({})",
                ws.name,
                path.display()
            );
            self.load_workspace(&ws.uri);
        }
        for ws in &params.event.removed {
            let Some(path) = uri_to_path(&ws.uri) else {
                continue;
            };
            debug!(
                "[{SERVER_LOG_TAG}][ws-folders] - {} ({})",
                ws.name,
                path.display()
            );
            // Drop every project rooted inside (or at) this folder.
            let drop_roots: Vec<PathBuf> = self
                .projects
                .keys()
                .filter(|r| r.starts_with(&path))
                .cloned()
                .collect();
            for root in drop_roots {
                self.drop_project(&root)?;
            }
            // Drop matching orphans too — they belonged to this
            // workspace folder and can't survive its removal.
            let orphan_uris: Vec<Uri> = self
                .orphans
                .iter()
                .filter_map(|(uri, _)| {
                    let p = uri_to_path(uri)?;
                    if p.starts_with(&path) {
                        Some(uri.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for uri in orphan_uris {
                let _ = self.orphans.remove(&uri);
                let _ = self.publish_diagnostics(uri, Vec::new(), None);
            }
            // Drop the workspace root itself.
            self.workspace_roots.retain(|w| w != &path);
        }
        Ok(())
    }

    // P32.8
    /// Tear down a single project: clear diagnostics for every URI it
    /// owned, drop the URI bindings, and remove the project from
    /// `self.projects`. Other projects sharing a URI in the
    /// multi-owner case keep their binding intact (see `unbind_uri`).
    fn drop_project(&mut self, root: &Path) -> Result<()> {
        let Some(project) = self.projects.remove(root) else {
            return Ok(());
        };
        let uris: Vec<Uri> = project.manager.iter().map(|(u, _)| u.clone()).collect();
        drop(project);
        for uri in uris {
            self.unbind_uri(&uri, root);
            // Only clear the editor's diagnostic list when nothing
            // else owns the URI; otherwise let the remaining owner's
            // publish stand.
            if !self.uri_owner.contains_key(&uri) {
                let _ = self.publish_diagnostics(uri, Vec::new(), None);
            }
        }
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
        // P32.7 — bucket reload triggers per project so a watcher
        // event in projectA never wakes up projectB.
        let mut reload_set: FxHashSet<PathBuf> = FxHashSet::default();
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
                // path = <proj_root>/lib/installed; project root is
                // path.parent().parent(). Only schedule a reload when
                // the implied root maps to a loaded project; events
                // for unknown projects are ignored.
                if let Some(root) = path
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
                    && let Some(project) = self.projects.get(&root)
                {
                    debug!(
                        "[{tag}][watch] {:?} on lib/installed -> reload",
                        ev.typ,
                        tag = project.tag
                    );
                    reload_set.insert(root);
                }
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
                let tag = self
                    .project_for(uri)
                    .map(|p| p.tag.as_str())
                    .unwrap_or_else(|| SERVER_LOG_TAG);
                debug!(
                    "[{tag}][watch] {:?} on opened {} -> skip",
                    ev.typ,
                    uri.as_str()
                );
                continue;
            }
            // Find which project(s) the file belongs to. If the file
            // has no owner yet (CREATED on a new module), fall back to
            // a parent-walk against the workspace folders. Files
            // outside every workspace folder are ignored — orphans
            // don't trigger reloads.
            let mut owners: Vec<PathBuf> = self
                .uri_owner
                .get(uri)
                .map(|o| o.to_vec())
                .unwrap_or_default();
            if owners.is_empty()
                && let Some(root) = find_owning_project_root(uri, &self.workspace_roots)
            {
                owners.push(root);
            }
            if owners.is_empty() {
                debug!("[watch] {:?} on unowned {} -> skip", ev.typ, uri.as_str());
                continue;
            }
            let owner_tags = self.owners_tag_csv(&owners);
            match ev.typ {
                FileChangeType::CREATED => {
                    debug!("[{owner_tags}][watch] created {} -> reload", uri.as_str());
                    for root in &owners {
                        reload_set.insert(root.clone());
                    }
                }
                FileChangeType::CHANGED => {
                    // Refresh the closed source from disk for every
                    // owning project. A change to a non-pragma module
                    // doesn't need a re-walk; an `invalidate` per
                    // project is enough.
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    for root in &owners {
                        if let Some(project) = self.projects.get_mut(root) {
                            project
                                .manager
                                .add_simple(uri.clone(), text.clone(), "project", false);
                        }
                    }
                    self.invalidate_with_slow_warning(uri, "watch");
                    if let Err(e) = self.publish_for(uri) {
                        warn!(
                            "[{owner_tags}][watch] publish_for({}) failed: {e}",
                            uri.as_str()
                        );
                    }
                }
                FileChangeType::DELETED => {
                    debug!("[{owner_tags}][watch] deleted {} -> reload", uri.as_str());
                    for root in &owners {
                        if let Some(project) = self.projects.get_mut(root) {
                            let _ = project.manager.remove(uri);
                        }
                        reload_set.insert(root.clone());
                    }
                    self.uri_owner.remove(uri);
                    self.publish_diagnostics(uri.clone(), Vec::new(), None)?;
                }
                _ => {}
            }
        }
        for root in reload_set {
            self.reload_project_closure_for(&root);
        }
        Ok(())
    }

    /// Returns the tag of the associated project if found,
    /// or the orphan if found, or server as fallback
    fn tag_for(&self, uri: &Uri) -> &str {
        self.project_for(uri)
            .map(|p| p.tag.as_str())
            .unwrap_or_else(|| {
                if self.orphans.get(uri).is_some() {
                    ORPHAN_LOG_TAG
                } else {
                    SERVER_LOG_TAG
                }
            })
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
                orphans: SourceManager::new(),
                workspace_roots: Vec::new(),
                lint_libs: false,
                registry: None,
                encoding: SourceEncoding::UTF8,
                last_analyzer_publish: FxHashMap::default(),
                pending_trailing: FxHashMap::default(),
                last_analyzer_diags: FxHashMap::default(),
                diagnostics_debounce: DIAGNOSTICS_DEBOUNCE_DEFAULT,
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
            .insert(root_a.clone(), Project::new(root_a.clone(), "projA".into()));
        b.projects
            .insert(root_b.clone(), Project::new(root_b.clone(), "projB".into()));

        let a1 = uri("file:///ws/projA/main.gcl");
        let b1 = uri("file:///ws/projB/main.gcl");
        b.bind_uri(a1.clone(), root_a.clone());
        b.bind_uri(b1.clone(), root_b.clone());

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

    // P32.9
    /// `compute_project_tag` shape covers: (a) project under a
    /// workspace folder → relative path tag; (b) project AT the
    /// workspace folder root → workspace folder basename; (c) project
    /// outside every workspace folder → root basename; (d) implicit
    /// empty-root project → `"single-file"`.
    #[test]
    fn project_tag_resolution() {
        let ws = PathBuf::from("/ws/repo");
        let ws_roots = std::slice::from_ref(&ws);

        // (a) under workspace folder
        let inner = ws.join("clientA").join("api");
        assert_eq!(compute_project_tag(&inner, ws_roots), "clientA/api");

        // (b) AT the workspace folder root
        assert_eq!(compute_project_tag(&ws, ws_roots), "repo");

        // (c) outside every workspace folder
        let stray = PathBuf::from("/tmp/loose-project");
        assert_eq!(compute_project_tag(&stray, ws_roots), "loose-project");

        // (d) implicit empty-root project
        assert_eq!(compute_project_tag(&PathBuf::new(), &[]), "single-file");
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

    // P32.7
    /// Fixture: two sibling project directories with their own
    /// `project.gcl`. projA `@include`s `src/` so new files dropped
    /// there get picked up on reload. Returns the temp base + both
    /// project roots.
    fn fixture_sibling_projects(slug: &str) -> (PathBuf, PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "gca_watcher_{}_{}_{}",
            slug,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let proj_a = tmp.join("projA");
        let proj_b = tmp.join("projB");
        let proj_a_src = proj_a.join("src");
        std::fs::create_dir_all(&proj_a_src).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        std::fs::write(proj_a.join("project.gcl"), "@include(\"src\");\n").unwrap();
        std::fs::write(proj_b.join("project.gcl"), "fn b(): int { return 2; }\n").unwrap();
        (tmp, proj_a, proj_b)
    }

    // P32.7
    /// A `CREATED` event under `projA/src/` triggers a reload of
    /// projA only; projB is untouched. Concretely: after the
    /// watcher fires, the new file lives in projA's manager and NOT
    /// in projB's.
    #[test]
    fn watcher_routes_created_file_to_owning_project() {
        let (tmp, proj_a, proj_b) = fixture_sibling_projects("created");
        let (mut b, _rx) = backend();
        b.workspace_roots.push(tmp.clone());
        b.load_workspace(&path_uri(&proj_a));
        b.load_workspace(&path_uri(&proj_b));

        // Drop a new file in projA/src/ on disk, then fire the watcher.
        let extra = proj_a.join("src").join("extra.gcl");
        std::fs::write(&extra, "fn extra(): int { return 0; }\n").unwrap();
        let extra_uri = path_uri(&extra);
        b.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: extra_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        })
        .unwrap();

        let proj_a_state = b.projects.get(&proj_a).expect("projA loaded");
        let proj_b_state = b.projects.get(&proj_b).expect("projB loaded");
        assert!(
            proj_a_state.manager.get(&extra_uri).is_some(),
            "projA's manager must now contain extra.gcl"
        );
        assert!(
            proj_b_state.manager.get(&extra_uri).is_none(),
            "projB's manager must NOT contain extra.gcl"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // P32.7
    /// A `lib/installed` write under `projA/` reloads projA only.
    /// We can't easily observe "did not reload projB", so the
    /// assertion is structural: the per-event routing logic picks
    /// the right project root.
    #[test]
    fn watcher_routes_lib_installed_to_owning_project() {
        let (tmp, proj_a, proj_b) = fixture_sibling_projects("installed");
        let (mut b, _rx) = backend();
        b.workspace_roots.push(tmp.clone());
        b.load_workspace(&path_uri(&proj_a));
        b.load_workspace(&path_uri(&proj_b));

        // Add a dummy file to projB so we can later observe that
        // projB's manager state didn't get clobbered by an unrelated
        // reload.
        let b_only = proj_b.join("b_only.gcl");
        std::fs::write(&b_only, "fn b_only(): int { return 9; }\n").unwrap();
        let b_only_uri = path_uri(&b_only);
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: b_only_uri.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "fn b_only(): int { return 9; }\n".into(),
            },
        })
        .unwrap();

        // Simulate `greycat install` populating projA/lib/installed.
        let lib_dir = proj_a.join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        let installed = lib_dir.join("installed");
        std::fs::write(&installed, "").unwrap();
        b.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: path_uri(&installed),
                typ: FileChangeType::CHANGED,
            }],
        })
        .unwrap();

        // projB still has b_only — its closure wasn't disturbed.
        let proj_b_state = b.projects.get(&proj_b).expect("projB loaded");
        assert!(
            proj_b_state.manager.get(&b_only_uri).is_some(),
            "projB must still own b_only after a lib/installed reload of projA"
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
        assert_eq!(
            b.uri_owner.get(&bar_uri).and_then(|o| o.first()),
            Some(&outer)
        );
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
        assert_eq!(
            b.uri_owner.get(&foo_uri).and_then(|o| o.first()),
            Some(&inner)
        );
        assert!(
            b.projects.contains_key(&inner),
            "opening outer/sub/foo.gcl must spawn inner project"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
