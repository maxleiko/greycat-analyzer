use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceEncoding;
use greycat_analyzer_core::module_desc::parse_module_desc;
use greycat_analyzer_core::path_to_uri;
use greycat_analyzer_core::registry::RegistryFetcher;
use greycat_analyzer_core::resolver::{Context, FsContext, global_std_dir};
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
use crate::watcher::{self, FsWatcher, is_gcl, is_installed_manifest, is_relevant};

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
    /// Build a project whose loader shares `ctx` with the owning
    /// [`Backend`], so the manager's recursive `@library` / `@include`
    /// walk and the Backend's own entrypoint / discovery checks resolve
    /// paths through the same filesystem view (real or mocked).
    pub fn new(root: PathBuf, tag: String, ctx: Arc<dyn Context>) -> Self {
        Self {
            root,
            manager: SourceManager::with_context(ctx),
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
    // P34.1
    /// In-process `notify` watcher. `None` when it failed to start
    /// (sandbox without inotify, watch exhaustion, …) — the server
    /// then runs exactly as before, leaning on the editor's
    /// `didChangeWatchedFiles`. Held here so the OS watch lives as
    /// long as the server.
    pub watcher: Option<FsWatcher>,
    // P34.2
    /// Roots currently registered with [`Self::watcher`], recursive.
    /// Diffed against the desired set on every project load / drop so
    /// watches track the loaded closure (see [`Self::resync_watch_roots`]).
    /// Empty when there's no watcher.
    pub watched_roots: FxHashSet<PathBuf>,
    // P34.3
    /// Debounce buffer: paths reported by the watcher since the last
    /// flush, pre-filtered to `.gcl` / `lib/installed`. Coalesced to a
    /// set so a `greycat install` burst collapses to one flush.
    pub pending_fs_events: FxHashSet<PathBuf>,
    // P34.3
    /// Deadline at which [`Self::pending_fs_events`] flushes. Pushed
    /// out on every fresh event (trailing debounce) so a contiguous
    /// burst flushes once it settles. `None` when the buffer is empty.
    pub fs_flush_deadline: Option<Instant>,
    // P34.2
    /// Resolved `$GREYCAT_HOME` (or `$HOME/.greycat`), captured once at
    /// startup. Drives the global `<home>/lib/std` watch root. `None`
    /// when the home couldn't be resolved (then std lives only under a
    /// project's local `lib/std`, already covered by the project watch).
    pub greycat_home: Option<PathBuf>,
    /// Filesystem view shared with every [`Project`]'s loader. Real
    /// [`FsContext`] in production; an in-memory mock in tests. Every
    /// path check on the request-dispatch path (entrypoint
    /// canonicalization, project discovery) goes through this so
    /// dispatch and loading agree on what exists. The OS-watcher
    /// subsystem stays on the real filesystem — it only runs against a
    /// live `notify` watch, which no in-memory context can drive.
    pub ctx: Arc<dyn Context>,
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
        let mut fast = parse_diagnostics(doc.root_node(), &doc.text, self.encoding);
        // P15.5 — pragma resolution diagnostics. Recomputed on every
        // publish so edits to `@include` / `@library` pragmas reflect
        // immediately. Anchored to the owning project's root. Skipped
        // for the implicit empty-root project (P32.1 lazy fallback).
        if !project.root.as_os_str().is_empty() {
            let desc = parse_module_desc(uri.clone(), &doc.text, doc.root_node());
            if let Ok(ctx) = FsContext::new() {
                fast.extend(pragma_diagnostics(
                    &doc.text,
                    &desc,
                    &project.root,
                    &ctx,
                    self.encoding,
                ));
            }
        }
        // P32.6 — multi-project-owner advisory.
        if let Some(owners) = self.uri_owner.get(uri)
            && owners.len() > 1
        {
            fast.push(multi_project_owner_diagnostic(
                &doc.text,
                owners,
                self.encoding,
            ));
        }
        // P33.1 — `missing-std` overlay on the entrypoint when the
        // resolver couldn't find std.
        if project.std_resolution == StdResolution::Missing
            && project.entrypoint_uri.as_ref() == Some(uri)
        {
            fast.push(missing_std_diagnostic(&doc.text, self.encoding));
        }
        // `duplicate-module-name` overlay on stem-colliding files.
        if let Some((name, existing)) = project.analysis.index.duplicate_modules.get(uri) {
            let module_name = &project.analysis.index.symbols[*name];
            fast.push(duplicate_module_name_diagnostic(
                &doc.text,
                module_name,
                existing,
                self.encoding,
            ));
        }
        let analyzer = project
            .analysis
            .module(uri)
            .map(|m| diagnostics_from_module(&doc.text, m, self.lint_libs, self.encoding))
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
            let mut diags = parse_diagnostics(doc.root_node(), &doc.text, self.encoding);
            diags.push(orphan_module_diagnostic(&doc.text, self.encoding));
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

    // P34.3
    /// Earliest deadline the main loop's `select!` timeout-arm must wake
    /// for: the sooner of a pending trailing analyzer publish and a
    /// pending filesystem-event flush. `None` when neither is scheduled
    /// (the loop falls back to a long idle timer).
    pub fn next_deadline(&self) -> Option<Instant> {
        match (self.next_trailing_deadline(), self.fs_flush_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    // P34.3
    /// Buffer one raw notify event into the debounce window. Runs on the
    /// main thread (the watcher thread only forwards). Irrelevant paths
    /// (non-`.gcl`, non-`lib/installed`) and pure access events are
    /// dropped here so the buffer only ever holds paths worth a reload.
    /// The flush deadline is pushed out on every fresh event so a
    /// contiguous burst (a `greycat install`) coalesces into one flush.
    pub fn on_fs_event(&mut self, res: watcher::RawFsEvent) {
        let event = match res {
            Ok(ev) => ev,
            Err(e) => {
                warn!("[{SERVER_LOG_TAG}][watch] notify error: {e}");
                return;
            }
        };
        // Access events (reads, opens) never change content — ignore.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        let mut buffered = false;
        for path in event.paths {
            if is_relevant(&path) {
                self.pending_fs_events.insert(path);
                buffered = true;
            }
        }
        if buffered {
            self.fs_flush_deadline = Some(Instant::now() + watcher::FS_DEBOUNCE_DEFAULT);
        }
    }

    // P34.3
    /// Flush the debounce buffer once its deadline has passed: classify
    /// each buffered path by re-statting the disk (notify's per-event
    /// kind is unreliable across platforms, so we derive CREATED /
    /// CHANGED / DELETED from current existence + whether the URI is
    /// already loaded) and hand the batch to the shared
    /// [`Self::apply_fs_changes`] processor. A no-op until the deadline
    /// is due, so the main loop can call it unconditionally.
    pub fn flush_fs_events(&mut self) {
        let Some(deadline) = self.fs_flush_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.fs_flush_deadline = None;
        if self.pending_fs_events.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut self.pending_fs_events);
        let mut changes: Vec<FileEvent> = Vec::with_capacity(paths.len());
        for path in paths {
            // Mirror the loader's URI formatting: canonicalize-or-self
            // then `path_to_uri`, so the synthesized URI matches the key
            // the manager stored for an already-loaded file.
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            let uri = path_to_uri(&canonical);
            let typ = if !path.exists() {
                FileChangeType::DELETED
            } else if self.uri_owner.contains_key(&uri) {
                FileChangeType::CHANGED
            } else {
                FileChangeType::CREATED
            };
            changes.push(FileEvent { uri, typ });
        }
        debug!(
            "[{SERVER_LOG_TAG}][watch] flushing {} debounced fs event(s)",
            changes.len()
        );
        if let Err(e) = self.apply_fs_changes(changes) {
            warn!("[{SERVER_LOG_TAG}][watch] apply_fs_changes failed: {e}");
        }
    }

    // P34.2
    /// The set of directories the in-process watcher should be watching
    /// right now, derived from current state: every existing workspace
    /// folder, the global `<greycat_home>/lib/std` (the std fallback,
    /// which lives outside any workspace folder), and every loaded
    /// project's local `lib/` subtree. Roots are canonicalized so the
    /// paths notify reports match the loader's canonical `uri_owner`
    /// keys, and contained roots are dropped (a project's `lib/` under a
    /// watched workspace folder is already covered recursively).
    ///
    /// Only existing directories are included: notify errors on a
    /// missing path, and a not-yet-created `lib/` (before the first
    /// `greycat install`) is covered by the enclosing workspace-folder
    /// watch, which sees the directory appear.
    fn compute_desired_roots(&self) -> FxHashSet<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for ws in &self.workspace_roots {
            if ws.is_dir() {
                roots.push(ws.canonicalize().unwrap_or_else(|_| ws.clone()));
            }
        }
        if let Some(home) = &self.greycat_home {
            let std_dir = global_std_dir(home);
            if std_dir.is_dir() {
                roots.push(std_dir.canonicalize().unwrap_or(std_dir));
            }
        }
        for project in self.projects.values() {
            if project.root.as_os_str().is_empty() {
                continue;
            }
            let lib = project.root.join("lib");
            if lib.is_dir() {
                roots.push(lib.canonicalize().unwrap_or(lib));
            }
        }
        dedup_contained(roots)
    }

    // P34.2
    /// Diff the desired watch-root set against what's currently watched
    /// and apply the delta: unwatch roots that dropped out (a project
    /// closed, a workspace folder removed) and watch the new ones. A
    /// no-op when there's no watcher (notify failed to start). Called
    /// after every structural change — eager load, lazy spawn, folder
    /// add/remove — so the watch set tracks the loaded closure.
    fn resync_watch_roots(&mut self) {
        if self.watcher.is_none() {
            return;
        }
        let desired = self.compute_desired_roots();
        let stale: Vec<PathBuf> = self
            .watched_roots
            .iter()
            .filter(|w| !desired.contains(*w))
            .cloned()
            .collect();
        for old in stale {
            if let Some(w) = self.watcher.as_mut() {
                let _ = watcher::unwatch(w, &old);
            }
            self.watched_roots.remove(&old);
            debug!("[{SERVER_LOG_TAG}][watch] unwatched {}", old.display());
        }
        for new in &desired {
            if self.watched_roots.contains(new) {
                continue;
            }
            let Some(w) = self.watcher.as_mut() else {
                return;
            };
            match watcher::watch(w, new) {
                Ok(()) => {
                    self.watched_roots.insert(new.clone());
                    info!("[{SERVER_LOG_TAG}][watch] watching {}", new.display());
                }
                Err(e) => warn!(
                    "[{SERVER_LOG_TAG}][watch] failed to watch {}: {e}",
                    new.display()
                ),
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
        // How the editor set up the project. A server launched without
        // `workspace_folders` never runs `load_workspace`, so its
        // projects load lazily on `did_open` — a different path worth
        // seeing in the log when triaging editor-specific behavior.
        #[allow(deprecated)]
        let root_uri = init.root_uri.as_ref().map(|u| u.as_str().to_string());
        info!(
            "[{SERVER_LOG_TAG}] initialize | workspace_folders={} | root_uri={root_uri:?}",
            init.workspace_folders
                .as_ref()
                .map(|w| w.len())
                .unwrap_or(0),
        );
        if let Some(workspaces) = init.workspace_folders.as_ref() {
            for ws in workspaces {
                info!(
                    "[{SERVER_LOG_TAG}] workspace_folder {} = {}",
                    ws.name,
                    ws.uri.as_str()
                );
                self.load_workspace(&ws.uri);
            }
        } else {
            info!("[{SERVER_LOG_TAG}] no workspace_folders — projects load lazily on did_open");
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
        // P34.2 — begin watching the eagerly-loaded closure (workspace
        // folders, global std, project libs). Independent of the editor
        // watcher above: the in-process watcher covers clients that
        // don't forward `didChangeWatchedFiles`.
        self.resync_watch_roots();
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
        if !self.ctx.is_file(&project_file) {
            debug!(
                "no project.gcl in workspace {} — skipping recursive load",
                ws_root.display()
            );
            return;
        }
        let project_root = ws_root.clone();
        let tag = compute_project_tag(&project_root, &self.workspace_roots);
        let mut project = Project::new(project_root.clone(), tag.clone(), self.ctx.clone());
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
        project.analysis.analyze_staged(&project.manager);
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
                    // Preserve the loader-assigned `lib` (e.g. "std")
                    // when re-opening an already-loaded module.
                    // `Document::new` defaults `lib` to "project";
                    // clobbering a library file's lib makes it lower
                    // under the wrong lib on the next rebuild, which
                    // breaks the `well_known.record("std","core",…)`
                    // gate and leaves `node_decl` (and every other
                    // std slot) stale against the re-interned symbols.
                    let doc = match project.manager.get(&uri) {
                        Some(existing) => {
                            let lib = existing.borrow().lib.clone();
                            Document::with_lib(params.text_document, lib, true)
                        }
                        None => Document::new(params.text_document),
                    };
                    debug!("[{}][did_open] {doc}", project.tag);
                    project.manager.add(doc);
                }
                self.invalidate_with_slow_warning(&uri, "did_open");
            }
            DocOwner::Orphan => {
                let doc = Document::new(params.text_document);
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
        let owner = find_owning_project_root(uri, &self.workspace_roots, self.ctx.as_ref());
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
                    let ctx = self.ctx.clone();
                    self.projects.entry(r.clone()).or_insert_with(|| {
                        Project::new(r.clone(), SINGLE_FILE_LOG_TAG.into(), ctx)
                    });
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
        let mut project = Project::new(root.to_path_buf(), tag.clone(), self.ctx.clone());
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
        project.analysis.analyze_staged(&project.manager);
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
        // P34.2 — a lazily-spawned project adds its local `lib/` to the
        // watch set (when not already covered by a workspace-folder watch).
        self.resync_watch_roots();
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
        let Ok(canonical) = self.ctx.canonicalize(&entrypoint) else {
            return false;
        };
        let Some(path) = uri_to_path(uri) else {
            return false;
        };
        self.ctx
            .canonicalize(&path)
            .map(|p| p == canonical)
            .unwrap_or(false)
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
        // Release the `project` borrow before the closure-membership
        // moves below — they shuffle `Document`s between `project.manager`
        // and `self.orphans`, which needs disjoint mutable access.
        let load_start = Instant::now();
        let (report, tag) = {
            let Some(project) = self.projects.get_mut(project_root) else {
                return;
            };
            let project_file = project.root.join("project.gcl");
            let report = project.manager.load_project(&project_file);
            project.std_resolution = report.std_resolution;
            project.entrypoint_uri = report.entrypoint_uri.clone();
            (report, project.tag.clone())
        };
        let load_took = load_start.elapsed();
        if report.std_resolution == StdResolution::Missing {
            warn!("[{tag}][reload_project] std not found (local or $HOME/.greycat)");
        }
        for lib in &report.unresolved_libraries {
            warn!(
                "[{tag}] unresolved @library('{lib}') in {}",
                project_root.display(),
            );
        }
        for err in &report.errors {
            warn!("[{tag}][reload_project] {err}");
        }
        #[allow(clippy::mutable_key_type)] // lsp_types::Uri as a HashSet key is fine in practice.
        let reachable: FxHashSet<Uri> = report.reachable.iter().cloned().collect();

        // Project ownership == membership in the reachable closure. Three
        // moves keep that invariant after a re-walk; `opened` decides only
        // whether a de-owned buffer survives as an orphan, never whether
        // it stays project-analyzed.
        //
        // (1) Re-home returning orphans: a reachable URI parked in the
        //     orphan bucket migrates back, its live buffer winning over
        //     the disk copy `load_project` just pulled in.
        let returning: Vec<Uri> = self
            .orphans
            .iter()
            .map(|(u, _)| u.clone())
            .filter(|u| reachable.contains(u))
            .collect();
        for uri in &returning {
            let doc = self.orphans.remove(uri);
            if let Some(doc) = doc
                && let Some(project) = self.projects.get_mut(project_root)
            {
                project.manager.add(doc);
            }
        }
        // (2) De-home departing buffers: an OPEN module no longer in the
        //     closure leaves the analysis set and becomes an orphan, its
        //     buffer preserved. Non-open unreachable modules are evicted
        //     outright in step 3.
        let leaving: Vec<Uri> = match self.projects.get(project_root) {
            Some(project) => project
                .manager
                .iter()
                .filter(|(u, cell)| !reachable.contains(u) && cell.borrow().opened)
                .map(|(u, _)| u.clone())
                .collect(),
            None => Vec::new(),
        };
        for uri in &leaving {
            let doc = self
                .projects
                .get_mut(project_root)
                .and_then(|p| p.manager.remove(uri));
            if let Some(doc) = doc {
                self.orphans.add(doc);
            }
            self.unbind_uri(uri, project_root);
        }
        // (3) Evict non-open unreachable modules outright.
        let evicted = match self.projects.get_mut(project_root) {
            Some(project) => project.manager.evict_unreachable(&reachable),
            None => Vec::new(),
        };

        let rebuild_start = Instant::now();
        if let Some(project) = self.projects.get_mut(project_root) {
            project.analysis.analyze_staged(&project.manager);
        }
        let rebuild_took = rebuild_start.elapsed();
        info!(
            "[{tag}][reload_project] {load_took:?} (parse+load) + {rebuild_took:?} (analyze) — closure: {} reachable, {} loaded, {} evicted, {} de-homed, {} re-homed",
            report.reachable.len(),
            report.loaded.len(),
            evicted.len(),
            leaving.len(),
            returning.len(),
        );

        let reachable_uris = report.reachable.clone();
        let owner_root = project_root.to_path_buf();

        // Re-index `uri_owner`: drop this project's binding for evicted
        // URIs (other projects' bindings survive — see P32.6), bind the
        // reachable closure.
        for uri in &evicted {
            self.unbind_uri(uri, &owner_root);
        }
        for uri in &reachable_uris {
            self.bind_uri(uri.clone(), owner_root.clone());
        }

        // Clear diagnostics for evicted URIs so stale editor entries go.
        for uri in &evicted {
            if let Err(e) = self.publish_diagnostics(uri.clone(), Vec::new(), None) {
                warn!("clear-diagnostics({}) failed: {e}", uri.as_str());
            }
        }
        // Republish de-homed buffers — now orphans, so they pick up the
        // orphan advisory and shed their stale project diagnostics.
        for uri in &leaving {
            if let Err(e) = self.publish_for(uri) {
                warn!("publish_for({}) failed: {e}", uri.as_str());
            }
        }
        // Republish the reachable closure: the rebuild may have changed
        // diagnostics on already-loaded files (e.g. a removed `@include`
        // turns a resolved name into an unknown one).
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
        // Project members stay analyzed when closed, so their cached
        // diagnostics stay valid; only orphans (no owning project) get
        // cleared, since theirs can't outlive the buffer.
        if self.project_for(&uri).is_none() {
            self.publish_diagnostics(uri, Vec::new(), None)?;
        }
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
        // P34.2 — folders added → their closures join the watch set;
        // folders removed → their roots (and now-dropped project libs)
        // fall out. One resync reconciles both.
        self.resync_watch_roots();
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
    /// The detailed processing lives in [`Self::apply_fs_changes`],
    /// shared with the in-process notify watcher (P34.4).
    pub fn did_change_watched_files(&mut self, params: DidChangeWatchedFilesParams) -> Result<()> {
        self.apply_fs_changes(params.changes)
    }

    // P34.4
    /// Apply a batch of filesystem change events, regardless of source.
    /// Both the editor's `didChangeWatchedFiles` and the in-process
    /// notify watcher's debounced flush funnel here, so there is exactly
    /// one reload code path. Re-running it for a duplicate event (both
    /// sources firing for the same change) is idempotent — closure
    /// reloads preserve already-loaded files and refresh-from-disk is a
    /// no-op when the bytes match.
    fn apply_fs_changes(&mut self, changes: Vec<FileEvent>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // P32.7 — bucket reload triggers per project so a watcher
        // event in projectA never wakes up projectB.
        let mut reload_set: FxHashSet<PathBuf> = FxHashSet::default();
        for ev in &changes {
            let uri = &ev.uri;
            let path = match uri_to_path(uri) {
                Some(p) => p,
                None => continue,
            };
            if is_installed_manifest(&path) {
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
            if !is_gcl(&path) {
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
                && let Some(root) =
                    find_owning_project_root(uri, &self.workspace_roots, self.ctx.as_ref())
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
                            // Preserve the existing library tag — a
                            // hardcoded "project" would reclassify std/lib
                            // files, defeating the `lint_libs` suppression
                            // gate and flooding the editor with library
                            // diagnostics on every `greycat install`.
                            let lib = project
                                .manager
                                .get(uri)
                                .map(|cell| cell.borrow().lib.clone())
                                .unwrap_or_else(|| "project".to_string());
                            project
                                .manager
                                .add_simple(uri.clone(), text.clone(), lib, false);
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
pub(crate) fn find_owning_project_root(
    uri: &Uri,
    workspace_roots: &[PathBuf],
    ctx: &dyn Context,
) -> Option<PathBuf> {
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
        if ctx.is_file(&cur.join("project.gcl")) {
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

// P34.2
/// Drop any path that is contained in another path in the set, so we
/// never register overlapping recursive watches (which would deliver
/// duplicate events and waste watch descriptors). A `lib/` under a
/// watched workspace folder, for instance, falls away — the folder
/// watch already covers it recursively.
fn dedup_contained(roots: Vec<PathBuf>) -> FxHashSet<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for r in roots {
        if !unique.contains(&r) {
            unique.push(r);
        }
    }
    unique
        .iter()
        .filter(|r| {
            !unique
                .iter()
                .any(|other| other.as_path() != r.as_path() && r.starts_with(other))
        })
        .cloned()
        .collect()
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
    use std::{
        io::{Error, ErrorKind},
        str::FromStr,
        sync::Mutex,
    };

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).unwrap()
    }

    /// Real-filesystem context for routing/watcher unit tests that set
    /// up actual temp dirs. The empty greycat-home is fine — none of
    /// these tests cross into `lib/std`.
    fn test_fs_ctx() -> Arc<dyn Context> {
        Arc::new(FsContext::with_greycat_home(PathBuf::new()))
    }

    /// Test backend bundled with the receiving end of its publish
    /// channel — the receiver MUST outlive every `publish_*` call,
    /// otherwise `Sender::send` errors out.
    fn backend() -> (Backend, Receiver<Message>) {
        backend_with(test_fs_ctx(), Vec::new())
    }

    /// `backend()` with an explicit filesystem context + workspace
    /// roots, for hermetic tests that drive the project-discovery /
    /// entrypoint-reload path through an in-memory [`Context`].
    fn backend_with(
        ctx: Arc<dyn Context>,
        workspace_roots: Vec<PathBuf>,
    ) -> (Backend, Receiver<Message>) {
        let (tx, rx) = unbounded();
        (
            Backend {
                client: tx,
                projects: FxHashMap::default(),
                uri_owner: FxHashMap::default(),
                orphans: SourceManager::with_context(ctx.clone()),
                workspace_roots,
                lint_libs: false,
                registry: None,
                encoding: SourceEncoding::UTF8,
                last_analyzer_publish: FxHashMap::default(),
                pending_trailing: FxHashMap::default(),
                last_analyzer_diags: FxHashMap::default(),
                diagnostics_debounce: DIAGNOSTICS_DEBOUNCE_DEFAULT,
                // P34 — unit-test backend has no real watcher; the
                // event-processing path is driven by calling
                // `on_fs_event` / `flush_fs_events` / `apply_fs_changes`
                // directly. `watcher: None` also exercises the
                // failed-to-start fallback (resync is a no-op).
                watcher: None,
                watched_roots: FxHashSet::default(),
                pending_fs_events: FxHashSet::default(),
                fs_flush_deadline: None,
                greycat_home: None,
                ctx,
            },
            rx,
        )
    }

    /// In-memory [`Context`] for hermetic Backend tests — lets the
    /// project-discovery + entrypoint-reload path run without touching
    /// disk. `canonicalize` / `is_file` fall back to the trait defaults
    /// (identity / read-backed), which are correct here.
    #[derive(Default)]
    struct MemContext {
        files: Mutex<FxHashMap<PathBuf, String>>,
        dirs: Mutex<FxHashSet<PathBuf>>,
        greycat_home: PathBuf,
    }

    impl MemContext {
        fn add_file(&self, path: PathBuf, content: &str) {
            for ancestor in path.ancestors().skip(1) {
                self.dirs.lock().unwrap().insert(ancestor.to_path_buf());
            }
            self.files.lock().unwrap().insert(path, content.to_string());
        }
    }

    impl Context for MemContext {
        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::NotFound, path.display().to_string()))
        }

        fn iter_gcl(&self, dir: &Path) -> Vec<PathBuf> {
            let mut out: Vec<PathBuf> = self
                .files
                .lock()
                .unwrap()
                .keys()
                .filter(|p| p.starts_with(dir))
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("gcl"))
                .cloned()
                .collect();
            out.sort();
            out
        }

        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.lock().unwrap().contains(path)
        }

        fn greycat_home(&self) -> &Path {
            &self.greycat_home
        }
    }

    /// Drain every pending publish and return the most recent
    /// diagnostic set for `target`.
    fn latest_diags_for(rx: &Receiver<Message>, target: &Uri) -> Option<Vec<Diagnostic>> {
        let mut latest = None;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(n) = msg
                && n.method == PublishDiagnostics::METHOD
            {
                let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                if &p.uri == target {
                    latest = Some(p.diagnostics);
                }
            }
        }
        latest
    }

    /// Removing an `@include` from `project.gcl` must re-walk the
    /// closure, evict the included module, and surface the now-unknown
    /// type it declared — driven end-to-end through `did_open` +
    /// `did_change` on an in-memory filesystem.
    #[test]
    fn removing_include_makes_entrypoint_type_unresolved() {
        let ctx = MemContext::default();
        ctx.add_file(
            PathBuf::from("/proj/project.gcl"),
            "@include(\"src\");\n\nfn foo(_: Foo) {}\n",
        );
        ctx.add_file(PathBuf::from("/proj/src/other.gcl"), "type Foo {}\n");
        let ctx: Arc<dyn Context> = Arc::new(ctx);

        let (mut b, rx) = backend_with(ctx, vec![PathBuf::from("/proj")]);
        let entry = uri("file:///proj/project.gcl");

        // Open project.gcl — discovers + loads the project (incl.
        // src/other.gcl) and publishes diagnostics.
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: entry.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "@include(\"src\");\n\nfn foo(_: Foo) {}\n".into(),
            },
        })
        .unwrap();
        let initial = latest_diags_for(&rx, &entry).expect("entrypoint published");
        assert!(
            !initial.iter().any(|d| d.message.contains("Foo")),
            "`Foo` must resolve while the include is present, got: {initial:?}"
        );

        // Remove the `@include` line (full-text replace, the LSP's
        // entrypoint-edit path).
        b.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: entry.clone(),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn foo(_: Foo) {}\n".into(),
            }],
        })
        .unwrap();
        let after = latest_diags_for(&rx, &entry).expect("entrypoint re-published");
        assert!(
            after.iter().any(|d| d.message.contains("Foo")),
            "`Foo` must be unresolved after the include is removed, got: {after:?}"
        );
    }

    /// The reported bug: with the included module OPEN in a buffer,
    /// removing the `@include` must still de-own it (move it to the
    /// orphan bucket) so its type stops resolving in the entrypoint —
    /// `opened` only preserves the buffer, it doesn't keep the module in
    /// the project's analysis closure.
    #[test]
    fn removing_include_with_open_member_de_homes_it() {
        let ctx = MemContext::default();
        ctx.add_file(
            PathBuf::from("/proj/project.gcl"),
            "@include(\"src\");\n\nfn foo(_: Foo) {}\n",
        );
        ctx.add_file(PathBuf::from("/proj/src/other.gcl"), "type Foo {}\n");
        let ctx: Arc<dyn Context> = Arc::new(ctx);

        let (mut b, rx) = backend_with(ctx, vec![PathBuf::from("/proj")]);
        let entry = uri("file:///proj/project.gcl");
        let member = uri("file:///proj/src/other.gcl");

        let open = |u: &Uri, text: &str| DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: u.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: text.into(),
            },
        };
        b.did_open(open(&entry, "@include(\"src\");\n\nfn foo(_: Foo) {}\n"))
            .unwrap();
        // Open the included member so it sits in a live buffer.
        b.did_open(open(&member, "type Foo {}\n")).unwrap();
        assert!(
            b.project_for(&member).is_some(),
            "the open member starts owned by the project"
        );

        b.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: entry.clone(),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn foo(_: Foo) {}\n".into(),
            }],
        })
        .unwrap();

        let after = latest_diags_for(&rx, &entry).expect("entrypoint re-published");
        assert!(
            after.iter().any(|d| d.message.contains("Foo")),
            "`Foo` must be unresolved once the open member is de-homed, got: {after:?}"
        );
        assert!(
            b.orphans.get(&member).is_some(),
            "the de-homed member must land in the orphan bucket (buffer preserved)"
        );
        assert!(
            b.project_for(&member).is_none(),
            "the de-homed member must no longer be project-owned"
        );
    }

    /// Same scenario as the in-memory test, but against a real on-disk
    /// project with the production [`FsContext`] — so the real
    /// `canonicalize` / `is_file` run through the entrypoint-reload gate.
    /// Rules out an identity-canonicalize mock masking a real-fs path
    /// mismatch in `is_project_entrypoint`.
    #[test]
    fn removing_include_makes_entrypoint_type_unresolved_on_disk() {
        let tmp = std::env::temp_dir().join(format!(
            "gca_include_eviction_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(
            tmp.join("project.gcl"),
            "@include(\"src\");\n\nfn foo(_: Foo) {}\n",
        )
        .unwrap();
        std::fs::write(tmp.join("src/other.gcl"), "type Foo {}\n").unwrap();
        // Canonicalize so the URI we build matches the loader's keys
        // (the loader canonicalizes every path before `path_to_uri`).
        let root = tmp.canonicalize().unwrap();

        let (mut b, rx) = backend_with(test_fs_ctx(), vec![root.clone()]);
        let entry = path_to_uri(&root.join("project.gcl"));

        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: entry.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "@include(\"src\");\n\nfn foo(_: Foo) {}\n".into(),
            },
        })
        .unwrap();
        let initial = latest_diags_for(&rx, &entry).expect("entrypoint published");
        assert!(
            !initial.iter().any(|d| d.message.contains("Foo")),
            "`Foo` must resolve while the include is present, got: {initial:?}"
        );

        b.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: entry.clone(),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn foo(_: Foo) {}\n".into(),
            }],
        })
        .unwrap();
        let after = latest_diags_for(&rx, &entry).expect("entrypoint re-published");
        let unresolved = after.iter().any(|d| d.message.contains("Foo"));

        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            unresolved,
            "`Foo` must be unresolved after the include is removed, got: {after:?}"
        );
    }

    // P32.1
    /// `project_for` routes by `uri_owner`. Two projects coexist;
    /// each URI dispatches to its bound root and never bleeds across.
    #[test]
    fn project_for_routes_by_uri_owner() {
        let (mut b, _rx) = backend();
        let root_a = PathBuf::from("/ws/projA");
        let root_b = PathBuf::from("/ws/projB");
        b.projects.insert(
            root_a.clone(),
            Project::new(root_a.clone(), "projA".into(), test_fs_ctx()),
        );
        b.projects.insert(
            root_b.clone(),
            Project::new(root_b.clone(), "projB".into(), test_fs_ctx()),
        );

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

    /// `did_close` keeps a project member's diagnostics (still in the
    /// analyzed closure) and clears only an orphan's (nothing backs it
    /// once the buffer closes). Regression guard for VSCode's
    /// preview-editor `didClose`-on-navigate wiping the just-left file.
    #[test]
    fn did_close_keeps_project_diags_clears_orphan() {
        let (mut b, rx) = backend();
        let root = PathBuf::from("/ws/proj");
        b.projects.insert(
            root.clone(),
            Project::new(root.clone(), "proj".into(), test_fs_ctx()),
        );
        let member = uri("file:///ws/proj/main.gcl");
        b.bind_uri(member.clone(), root.clone());
        let orphan = uri("file:///elsewhere/loose.gcl");

        // Seed both with a live diagnostic (mimics a prior publish the
        // editor is already showing).
        let diag = Diagnostic::new_simple(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            "boom".into(),
        );
        b.publish_diagnostics(member.clone(), vec![diag.clone()], Some(1))
            .unwrap();
        b.publish_diagnostics(orphan.clone(), vec![diag], Some(1))
            .unwrap();

        let close = |u: Uri| DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: u },
        };
        b.did_close(close(member.clone())).unwrap();
        b.did_close(close(orphan.clone())).unwrap();

        // Drain once (shared channel) into the latest set per URI.
        let mut member_latest: Option<Vec<Diagnostic>> = None;
        let mut orphan_latest: Option<Vec<Diagnostic>> = None;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(n) = msg
                && n.method == PublishDiagnostics::METHOD
            {
                let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                if p.uri == member {
                    member_latest = Some(p.diagnostics);
                } else if p.uri == orphan {
                    orphan_latest = Some(p.diagnostics);
                }
            }
        }

        // Member: only the seed publish; `did_close` left it untouched.
        assert_eq!(
            member_latest.map(|d| d.len()),
            Some(1),
            "closing a project member must keep its diagnostics"
        );
        // Orphan: seed publish followed by an empty clear on close.
        assert_eq!(
            orphan_latest.map(|d| d.len()),
            Some(0),
            "closing an orphan must clear its diagnostics with an empty publish"
        );
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

    /// Drain every queued `publishDiagnostics` notification, returning
    /// the LATEST diagnostic set seen for `uri`.
    fn latest_diags(rx: &Receiver<Message>, uri: &Uri) -> Vec<lsp_types::Diagnostic> {
        let mut latest = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(n) = msg
                && n.method == PublishDiagnostics::METHOD
            {
                let p: lsp_types::PublishDiagnosticsParams =
                    serde_json::from_value(n.params).unwrap();
                if &p.uri == uri {
                    latest = p.diagnostics;
                }
            }
        }
        latest
    }

    /// Repro the user report: editing the entrypoint `project.gcl`
    /// (which holds `node { 42 }`) makes a spurious
    /// `positional-object-init` appear that wasn't there on load.
    #[test]
    fn entrypoint_edit_node_false_positive() {
        let tmp = std::env::temp_dir().join(format!(
            "gca_node_repro_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let std_dir = tmp.join("lib").join("std");
        std::fs::create_dir_all(&std_dir).unwrap();
        std::fs::write(
            tmp.join("project.gcl"),
            "fn main() {\n    node { 42 };\n}\n",
        )
        .unwrap();
        // Real (non-symlinked) std so the canonical URI the loader
        // stores matches the URI `did_open` later sends.
        let core_src = "native type any {}\nnative type null {}\nnative type int {}\n\
             native type float {}\nnative type String {}\nnative type bool {}\n\
             native type Array<T> {}\nnative type Map<K, V> {}\nnative type node<T> {}\n";
        std::fs::write(std_dir.join("core.gcl"), core_src).unwrap();

        let (mut b, rx) = backend();
        b.workspace_roots.push(tmp.clone());
        let entry = path_uri(&tmp.join("project.gcl"));

        // 1. Workspace load (the "restart" — should be clean).
        b.load_workspace(&path_uri(&tmp));
        let on_load = latest_diags(&rx, &entry);
        let load_bad: Vec<_> = on_load
            .iter()
            .filter(|d| d.message.contains("positional initializers"))
            .collect();
        assert!(
            load_bad.is_empty(),
            "on load (restart) there must be no node diag: {:?}",
            on_load.iter().map(|d| &d.message).collect::<Vec<_>>()
        );

        // 2. Editor opens the entrypoint buffer.
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: entry.clone(),
                language_id: "greycat".into(),
                version: 1,
                text: "fn main() {\n    node { 42 };\n}\n".into(),
            },
        })
        .unwrap();

        // 2b. THE ZED-SPECIFIC STEP: the editor also opens the std
        // library file `lib/std/core.gcl`. `did_open` rebuilds the
        // Document with the default `lib="project"`, clobbering the
        // `lib="std"` the loader assigned — which is what later breaks
        // the `well_known.record` gate on rebuild.
        let core_uri = path_uri(&std_dir.join("core.gcl"));
        b.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: core_uri,
                language_id: "greycat".into(),
                version: 1,
                text: core_src.into(),
            },
        })
        .unwrap();

        // 2c. Zed also opens unrelated `.gcl` files in the tree, which
        // bind to this project and grow its module set. That perturbs
        // the symbol table re-interned on the next rebuild — which is
        // what makes the STALE `node_decl` (from 2b's clobber) actually
        // mismatch the freshly-resolved head. Files sort before
        // `lib/std/core.gcl`, so they shift `node`'s interned id.
        for n in 0..4 {
            let extra = tmp.join(format!("aaa_{n}.gcl"));
            std::fs::write(&extra, format!("type Extra{n} {{ field{n}: int; }}\n")).unwrap();
            b.did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: path_uri(&extra),
                    language_id: "greycat".into(),
                    version: 1,
                    text: format!("type Extra{n} {{ field{n}: int; }}\n"),
                },
            })
            .unwrap();
        }

        // The deterministic root-cause invariant: re-opening a library
        // file must NOT rewrite its `lib`. Pre-fix it was clobbered to
        // "project", which broke the well-known std gate on rebuild.
        let core_lib = b
            .project_for(&entry)
            .unwrap()
            .manager
            .get(&path_uri(&tmp.join("lib").join("std").join("core.gcl")))
            .map(|c| c.borrow().lib.clone());
        assert_eq!(
            core_lib.as_deref(),
            Some("std"),
            "did_open must preserve a library file's lib (was clobbered to {core_lib:?})"
        );

        // 3. The entrypoint edit → reload_project_closure → full rebuild.
        // With `lib` preserved, `node_decl` is re-recorded against the
        // freshly re-interned symbol table, so `node { 42 }` stays clean.
        b.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: entry.clone(),
                version: 2,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn main() {\n    node { 42 };\n\n}\n".into(),
            }],
        })
        .unwrap();
        let after_edit = latest_diags(&rx, &entry);
        let _ = std::fs::remove_dir_all(&tmp);
        let edit_bad: Vec<_> = after_edit
            .iter()
            .filter(|d| d.message.contains("positional initializers"))
            .map(|d| &d.message)
            .collect();
        assert!(
            edit_bad.is_empty(),
            "after editing the entrypoint, node must NOT raise positional-object-init: {after_edit:?}"
        );
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
        let ctx = FsContext::with_greycat_home(PathBuf::new());

        // File in inner dir → inner wins.
        let foo_uri = path_uri(&inner.join("foo.gcl"));
        assert_eq!(
            find_owning_project_root(&foo_uri, &workspace_roots, &ctx),
            Some(inner.clone())
        );

        // File in outer dir (sibling of `sub`) → outer wins (inner is
        // not on the walk path).
        let bar_uri = path_uri(&outer.join("bar.gcl"));
        assert_eq!(
            find_owning_project_root(&bar_uri, &workspace_roots, &ctx),
            Some(outer.clone())
        );

        // File outside every workspace folder → no owner.
        let elsewhere_uri = path_uri(&tmp.join("elsewhere.gcl"));
        assert_eq!(
            find_owning_project_root(&elsewhere_uri, &workspace_roots, &ctx),
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

    // =====================================================================
    // P34.5 — in-process notify watcher
    // =====================================================================

    /// Build a raw watcher payload for `path` with the given kind. Sets
    /// the public `paths` field directly (the builder's `attrs` field is
    /// private) so this doesn't depend on a particular notify builder.
    fn fs_event(kind: notify::EventKind, path: &Path) -> watcher::RawFsEvent {
        let mut ev = notify::Event::new(kind);
        ev.paths.push(path.to_path_buf());
        Ok(ev)
    }

    /// Drain the client channel and return the most recent
    /// `publishDiagnostics` payload for `uri` (if any).
    fn latest_published_diags(rx: &Receiver<Message>, uri: &Uri) -> Option<Vec<Diagnostic>> {
        let mut latest = None;
        while let Ok(msg) = rx.try_recv() {
            if let Message::Notification(n) = msg
                && n.method == "textDocument/publishDiagnostics"
            {
                let params: PublishDiagnosticsParams =
                    serde_json::from_value(n.params).expect("valid publishDiagnostics params");
                if &params.uri == uri {
                    latest = Some(params.diagnostics);
                }
            }
        }
        latest
    }

    /// Project with a local `@library("mylib")` whose single module
    /// carries a semantic error. Returns (tmp_root, project_root,
    /// lib_file_path, lib_installed_path).
    fn fixture_project_with_lib(slug: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "gca_libtag_{}_{}_{}",
            slug,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let proj = tmp.join("proj");
        let lib_dir = proj.join("lib").join("mylib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(proj.join("project.gcl"), "@library(\"mylib\", \"1.0\");\n").unwrap();
        // A return-type mismatch: a real semantic diagnostic that must
        // stay suppressed while the file is library-owned.
        let lib_file = lib_dir.join("types.gcl");
        std::fs::write(&lib_file, "fn helper(): int { return \"oops\"; }\n").unwrap();
        let installed = proj.join("lib").join("installed");
        std::fs::write(&installed, "mylib=1.0\n").unwrap();
        (tmp, proj, lib_file, installed)
    }

    // P34 — lib-tag regression
    /// Reproduces "diagnostics pile up after `greycat install`": an
    /// install rewrites every library file (CHANGED events). The watcher
    /// must refresh content WITHOUT reclassifying the file from its
    /// library tag (`mylib`) to `project` — otherwise the `lint_libs`
    /// suppression gate (`from_module` keys off `module.lib != "project"`)
    /// stops applying and the library's internal diagnostics flood the
    /// editor. The follow-up `lib/installed` closure reload must not
    /// resurrect the mistag either (`load_file` skips loaded files, so a
    /// bad tag would stick).
    #[test]
    fn changed_library_file_keeps_tag_and_stays_suppressed() {
        let (tmp, proj, lib_file, installed) = fixture_project_with_lib("changed");
        let (mut b, rx) = backend();
        b.workspace_roots.push(tmp.clone());
        b.load_workspace(&path_uri(&proj));

        let lib_uri = path_uri(&lib_file);
        let lib_tag = |b: &Backend| {
            b.projects
                .get(&proj)
                .unwrap()
                .manager
                .get(&lib_uri)
                .unwrap()
                .borrow()
                .lib
                .clone()
        };
        // Loaded as a library; its diagnostics are suppressed.
        assert_eq!(lib_tag(&b), "mylib");
        assert!(
            latest_published_diags(&rx, &lib_uri).is_none_or(|d| d.is_empty()),
            "library diagnostics must be suppressed on initial load"
        );

        // Simulate `greycat install`: rewrite the library file in place
        // (CHANGED) + touch lib/installed (triggers a closure reload).
        std::fs::write(&lib_file, "fn helper(): int { return \"still-bad\"; }\n").unwrap();
        b.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![
                FileEvent {
                    uri: lib_uri.clone(),
                    typ: FileChangeType::CHANGED,
                },
                FileEvent {
                    uri: path_uri(&installed),
                    typ: FileChangeType::CHANGED,
                },
            ],
        })
        .unwrap();

        // The fix: the tag survives the refresh + reload...
        assert_eq!(
            lib_tag(&b),
            "mylib",
            "a CHANGED event must not reclassify a library file as project-owned"
        );
        // ...so the user-visible symptom is gone: no library diagnostics.
        assert!(
            latest_published_diags(&rx, &lib_uri).is_none_or(|d| d.is_empty()),
            "library diagnostics must STAY suppressed after `greycat install`; \
             re-tagging to `project` is what made them pile up"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // P34.3
    /// `on_fs_event` buffers `.gcl` / `lib/installed` paths and arms the
    /// flush deadline; access events and irrelevant extensions are
    /// dropped without arming anything.
    #[test]
    fn on_fs_event_buffers_relevant_ignores_noise() {
        let (mut b, _rx) = backend();
        // Irrelevant extension: not buffered.
        b.on_fs_event(fs_event(
            notify::EventKind::Create(notify::event::CreateKind::Any),
            Path::new("/proj/src/readme.txt"),
        ));
        assert!(b.pending_fs_events.is_empty());
        assert!(b.fs_flush_deadline.is_none());

        // Access event on a .gcl: ignored (no content change).
        b.on_fs_event(fs_event(
            notify::EventKind::Access(notify::event::AccessKind::Any),
            Path::new("/proj/src/a.gcl"),
        ));
        assert!(b.pending_fs_events.is_empty());
        assert!(b.fs_flush_deadline.is_none());

        // A real .gcl change: buffered + deadline armed.
        let gcl = PathBuf::from("/proj/src/a.gcl");
        b.on_fs_event(fs_event(
            notify::EventKind::Modify(notify::event::ModifyKind::Any),
            &gcl,
        ));
        assert!(b.pending_fs_events.contains(&gcl));
        assert!(b.fs_flush_deadline.is_some());

        // lib/installed is relevant too.
        let installed = PathBuf::from("/proj/lib/installed");
        b.on_fs_event(fs_event(
            notify::EventKind::Create(notify::event::CreateKind::Any),
            &installed,
        ));
        assert!(b.pending_fs_events.contains(&installed));
    }

    // P34.3
    /// `flush_fs_events` is a no-op until the debounce deadline passes,
    /// so a partial burst isn't processed early.
    #[test]
    fn flush_fs_events_waits_for_deadline() {
        let (mut b, _rx) = backend();
        b.pending_fs_events.insert(PathBuf::from("/proj/src/a.gcl"));
        b.fs_flush_deadline = Some(Instant::now() + Duration::from_secs(30));
        b.flush_fs_events();
        // Deadline in the future → nothing drained.
        assert_eq!(b.pending_fs_events.len(), 1);
        assert!(b.fs_flush_deadline.is_some());
    }

    // P34.5
    /// The headline contract: a file dropped on disk is picked up via
    /// the watcher's debounced flush WITHOUT any editor-side
    /// `didChangeWatchedFiles`. Drive `on_fs_event` (what the select
    /// loop does on a real notify delivery) + `flush_fs_events`, never
    /// `did_change_watched_files`, and assert the new module lands in
    /// the owning project's manager.
    #[test]
    fn watcher_picks_up_new_file_without_editor_event() {
        let (tmp, proj_a, _proj_b) = fixture_sibling_projects("notify_pickup");
        let (mut b, _rx) = backend();
        b.workspace_roots.push(tmp.clone());
        b.load_workspace(&path_uri(&proj_a));

        // Drop a new module in projA/src on disk.
        let extra = proj_a.join("src").join("extra.gcl");
        std::fs::write(&extra, "fn extra(): int { return 7; }\n").unwrap();
        let extra_uri = path_uri(&extra);

        // Pre-condition: not yet known to the project.
        assert!(
            b.projects
                .get(&proj_a)
                .unwrap()
                .manager
                .get(&extra_uri)
                .is_none(),
            "extra.gcl must not be loaded before the watcher fires"
        );

        // Simulate the notify delivery + debounce flush (no editor event).
        b.on_fs_event(fs_event(
            notify::EventKind::Create(notify::event::CreateKind::File),
            &extra,
        ));
        // Force the debounce deadline due, then flush as the loop would.
        b.fs_flush_deadline = Some(Instant::now());
        b.flush_fs_events();

        assert!(
            b.projects
                .get(&proj_a)
                .unwrap()
                .manager
                .get(&extra_uri)
                .is_some(),
            "watcher flush must reload projA's closure and pick up extra.gcl"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // P34.4
    /// Failure-to-start fallback: with no watcher (the `None` state
    /// `start_watcher` returns when notify can't start), `resync` is a
    /// no-op, and the editor's `didChangeWatchedFiles` path keeps
    /// working — i.e. the server operates exactly as it did before P34.
    #[test]
    fn no_watcher_degrades_gracefully() {
        let (tmp, proj_a, _proj_b) = fixture_sibling_projects("notify_fallback");
        let (mut b, _rx) = backend();
        assert!(b.watcher.is_none());
        b.workspace_roots.push(tmp.clone());
        b.load_workspace(&path_uri(&proj_a));

        // resync without a watcher registers nothing and doesn't panic.
        b.resync_watch_roots();
        assert!(b.watched_roots.is_empty());

        // Editor-driven path still picks up a new file.
        let extra = proj_a.join("src").join("viaeditor.gcl");
        std::fs::write(&extra, "fn ve(): int { return 1; }\n").unwrap();
        let extra_uri = path_uri(&extra);
        b.did_change_watched_files(DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                uri: extra_uri.clone(),
                typ: FileChangeType::CREATED,
            }],
        })
        .unwrap();
        assert!(
            b.projects
                .get(&proj_a)
                .unwrap()
                .manager
                .get(&extra_uri)
                .is_some(),
            "editor didChangeWatchedFiles must still work when notify is absent"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // P34.2
    /// With a real watcher, `resync_watch_roots` registers the workspace
    /// folder and unregisters it when it drops out of the desired set.
    /// Skipped when notify can't start in the test sandbox.
    #[test]
    fn resync_registers_and_unregisters_real_roots() {
        let dir = std::env::temp_dir().join(format!(
            "gca_resync_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let (mut b, _rx) = backend();
        let (tx, _wrx) = unbounded();
        b.watcher = watcher::start_watcher(tx);
        if b.watcher.is_none() {
            // notify unavailable in this sandbox — nothing to assert.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        b.workspace_roots.push(dir.clone());
        b.resync_watch_roots();
        assert!(
            !b.watched_roots.is_empty(),
            "workspace folder should be watched after resync"
        );

        // Folder removed from the desired set → unwatched on next resync.
        b.workspace_roots.clear();
        b.resync_watch_roots();
        assert!(
            b.watched_roots.is_empty(),
            "watch set should be empty after the root drops out"
        );

        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // P34.2
    /// `dedup_contained` collapses duplicates and drops paths nested
    /// under another watched root, while keeping independent roots.
    #[test]
    fn dedup_contained_drops_nested_paths() {
        let roots = vec![
            PathBuf::from("/ws"),
            PathBuf::from("/ws/proj/lib"), // under /ws → dropped
            PathBuf::from("/home/.greycat/lib/std"), // independent → kept
            PathBuf::from("/ws"),          // dup → collapsed
        ];
        let out = dedup_contained(roots);
        assert!(out.contains(&PathBuf::from("/ws")));
        assert!(out.contains(&PathBuf::from("/home/.greycat/lib/std")));
        assert!(!out.contains(&PathBuf::from("/ws/proj/lib")));
        assert_eq!(out.len(), 2);
    }

    // P34.2
    /// The property that actually matters: containment is *component-wise*
    /// (`Path::starts_with`), NOT a string prefix. Siblings that share a
    /// textual prefix must both survive — a `str::starts_with`
    /// implementation would wrongly drop `/wsfoo` as "under" `/ws` and
    /// silently shrink the watch set. This is the test that fails if the
    /// containment check is ever refactored to string comparison.
    #[test]
    fn dedup_contained_keeps_sibling_prefixes() {
        let roots = vec![
            PathBuf::from("/ws"),
            PathBuf::from("/wsfoo"), // shares the text "/ws" but is a sibling
            PathBuf::from("/ws-2"),  // ditto
            PathBuf::from("/a/b"),
            PathBuf::from("/a/bc"), // sibling of /a/b, not nested under it
        ];
        let out = dedup_contained(roots.clone());
        // Nothing is nested under anything else → all five survive.
        assert_eq!(out.len(), 5, "no sibling should be dropped: {out:?}");
        for r in &roots {
            assert!(out.contains(r), "{} must be kept", r.display());
        }
    }

    // P34.2
    /// A containment chain collapses to its single topmost root, and the
    /// result is independent of input order — each element is tested
    /// against the whole set, not just earlier entries. (Also covers the
    /// nested-workspace-folder case: `/ws` + `/ws/inner` → just `/ws`.)
    #[test]
    fn dedup_contained_collapses_chains_order_independent() {
        let forward = vec![
            PathBuf::from("/a"),
            PathBuf::from("/a/b"),
            PathBuf::from("/a/b/c"),
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        let expected: FxHashSet<PathBuf> = [PathBuf::from("/a")].into_iter().collect();
        assert_eq!(dedup_contained(forward), expected);
        assert_eq!(dedup_contained(reversed), expected);
    }

    // P34.2
    /// Degenerate inputs: empty stays empty; wholly-independent roots are
    /// all preserved (no false containment between unrelated trees).
    #[test]
    fn dedup_contained_empty_and_independent() {
        assert!(dedup_contained(Vec::new()).is_empty());

        let roots = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c/d"),
        ];
        let out = dedup_contained(roots.clone());
        assert_eq!(out.len(), 3);
        for r in &roots {
            assert!(out.contains(r));
        }
    }

    // P34.3
    /// `next_deadline` is the earlier of a pending trailing publish and
    /// a pending fs flush.
    #[test]
    fn next_deadline_is_min_of_trailing_and_fs() {
        let (mut b, _rx) = backend();
        assert!(b.next_deadline().is_none());

        let fs_at = Instant::now() + Duration::from_secs(5);
        b.fs_flush_deadline = Some(fs_at);
        assert_eq!(b.next_deadline(), Some(fs_at));

        let trailing_at = Instant::now() + Duration::from_secs(1);
        b.pending_trailing
            .insert(path_uri(Path::new("/p/a.gcl")), trailing_at);
        // Trailing is sooner → wins.
        assert_eq!(b.next_deadline(), Some(trailing_at));
    }
}
