use std::{
    cell::{Ref, RefCell},
    path::{Path, PathBuf},
    sync::Arc,
};

// `web-time` is a transparent drop-in for `std::time` — re-exports
// the std types on native, falls back to `performance.now()` on
// `wasm32-unknown-unknown` (where `std::time::Instant::now()` panics
// with "time not implemented on this platform"). The crate's whole
// purpose is to be cfg-gated internally, so consumers don't need to.
use web_time::{Duration, Instant};

use lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Uri};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Document,
    module_desc::{ModuleDesc, parse_module_desc},
    resolver::{Context, FsContext, global_std_dir, library_dir},
};

#[derive(Debug, Clone, Copy)]
pub enum SourceEncoding {
    UTF8,
    UTF16,
}

/// Storage for parsed `.gcl` documents, keyed by LSP `Uri`. Holds a
/// [`Context`] so callers can trigger recursive loads (`@library` /
/// `@include`) without threading filesystem access themselves.
///
/// Ports `packages/lang/src/project/source_manager.ts` (sources/modules/
/// errors maps) plus the recursive-load slice of `analyze.ts` (cycle
/// detection, lib resolution).
pub struct SourceManager {
    documents: FxHashMap<Uri, RefCell<Document>>,
    ctx: Arc<dyn Context>,
    // P40.1
    /// URI of the project's entrypoint (`project.gcl` at the project
    /// root). Set by `load_project`, mirrors `LoadReport::entrypoint_uri`
    /// so downstream consumers (the analyzer's pragma walker, in
    /// particular) can distinguish the entrypoint from any other
    /// reachable module without threading the report through every
    /// call site.
    entrypoint_uri: Option<Uri>,
}

impl SourceManager {
    /// Construct a `SourceManager` over the real filesystem. Falls back to
    /// a dummy GreyCat home when `$GREYCAT_HOME` / `$HOME` are unresolvable
    /// — recursive loads that cross into `lib/std` will surface that as a
    /// missing-library at resolve time.
    pub fn new() -> Self {
        let ctx: Arc<dyn Context> = match FsContext::new() {
            Ok(c) => Arc::new(c),
            Err(_) => Arc::new(FsContext::with_greycat_home(PathBuf::new())),
        };
        Self::with_context(ctx)
    }

    pub fn with_context(ctx: Arc<dyn Context>) -> Self {
        Self {
            documents: FxHashMap::default(),
            ctx,
            entrypoint_uri: None,
        }
    }

    pub fn ctx(&self) -> &Arc<dyn Context> {
        &self.ctx
    }

    // P40.1
    /// Entrypoint URI captured by the most recent `load_project` call,
    /// or `None` if the manager was populated through `add` / `add_simple`
    /// only (the LSP / test paths). Cheap accessor — `O(1)`.
    pub fn entrypoint_uri(&self) -> Option<&Uri> {
        self.entrypoint_uri.as_ref()
    }

    pub fn add(&mut self, doc: Document) {
        self.documents.insert(doc.uri.clone(), RefCell::new(doc));
    }

    /// Convenience: build a `Document` from raw text and add it. Mirrors
    /// TS `addSimpleSource(uri, content, lib)`.
    pub fn add_simple(
        &mut self,
        uri: Uri,
        text: impl Into<String>,
        lib: impl Into<String>,
        opened: bool,
    ) {
        let doc = Document::with_lib(
            TextDocumentItem {
                uri,
                language_id: "greycat".into(),
                version: 0,
                text: text.into(),
            },
            lib,
            opened,
        );
        self.add(doc);
    }

    pub fn get(&self, uri: &Uri) -> Option<&RefCell<Document>> {
        self.documents.get(uri)
    }

    pub fn remove(&mut self, uri: &Uri) -> Option<Document> {
        self.documents.remove(uri).map(RefCell::into_inner)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Uri, &RefCell<Document>)> {
        self.documents.iter()
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    pub fn update(
        &mut self,
        uri: &Uri,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        encoding: SourceEncoding,
    ) -> Ref<'_, Document> {
        if let Some(doc) = self.documents.get(uri) {
            doc.borrow_mut().apply_changes(changes, version, encoding);
            doc.borrow()
        } else {
            panic!("cannot update unknown document")
        }
    }

    /// Recursively load a `project.gcl` and every module reachable through
    /// `@include` (project-local) and `@library` (under `<project_dir>/lib/`,
    /// or `<greycat_home>/lib/std/` for the global `std` fallback).
    ///
    /// Cycle-safe: each canonical filepath is parsed at most once. Returns
    /// the [`LoadReport`] with everything actually parsed in this load,
    /// including which `@library` declarations couldn't be resolved.
    ///
    // P1.4 — diagnostics deferred. P2 — project-graph data model.
    /// This is the recursive-load slice of TS `analyze.ts:resolve_*` —
    /// trimmed to path resolution + parsing.
    pub fn load_project(&mut self, project_filepath: &Path) -> LoadReport {
        let mut report = LoadReport::default();
        let project_dir = match project_filepath.parent() {
            Some(p) => p.to_path_buf(),
            None => {
                report.errors.push(format!(
                    "project path has no parent: {}",
                    project_filepath.display()
                ));
                return report;
            }
        };

        let mut visited: FxHashSet<PathBuf> = FxHashSet::default();
        // Load the project.gcl itself first.
        if let Some(uri) = self.load_file(project_filepath, "project", &mut visited, &mut report) {
            report.entrypoint_uri = Some(uri.clone());
            // P40.1 — mirror into the manager so the analyzer's pragma
            // walker can identify the entrypoint without threading the
            // LoadReport through every call.
            self.entrypoint_uri = Some(uri.clone());
            // Walk its mod_pragmas to find @library / @include.
            let desc = self.module_desc_for(&uri);
            self.process_includes(&project_dir, &desc, &mut visited, &mut report);
            self.process_libraries(&project_dir, &desc, &mut visited, &mut report);
        }
        // P33.1 — always ensure `std` is loaded, regardless of whether
        // the project.gcl declared `@library("std", ...)`. This mirrors
        // the GreyCat runtime: local `lib/std` wins, otherwise
        // `$HOME/.greycat/lib/std`, otherwise the runtime falls back to
        // its embedded definitions (which the analyzer can't see — we
        // surface that as `missing-std` on the entrypoint).
        report.std_resolution = self.ensure_std_loaded(&project_dir, &mut visited, &mut report);

        report
    }

    // P33.1
    /// Load the `std` library closure from the local `<project_dir>/lib/std/`
    /// if present, else from the global `<greycat_home>/lib/std/`. Returns
    /// which (if any) source was used. `load_file` is idempotent against
    /// `visited`, so this is safe to call after `process_libraries` even
    /// when `@library("std", ...)` was declared.
    fn ensure_std_loaded(
        &mut self,
        project_dir: &Path,
        visited: &mut FxHashSet<PathBuf>,
        report: &mut LoadReport,
    ) -> StdResolution {
        let local = library_dir(project_dir, "std");
        if self.ctx.is_dir(&local) {
            for path in self.ctx.iter_gcl(&local) {
                self.load_file(&path, "std", visited, report);
            }
            return StdResolution::Local;
        }
        let global = global_std_dir(self.ctx.greycat_home());
        if self.ctx.is_dir(&global) {
            for path in self.ctx.iter_gcl(&global) {
                self.load_file(&path, "std", visited, report);
            }
            return StdResolution::Global;
        }
        StdResolution::Missing
    }

    fn module_desc_for(&self, uri: &Uri) -> ModuleDesc {
        let Some(cell) = self.documents.get(uri) else {
            return ModuleDesc::default();
        };
        let doc = cell.borrow();
        parse_module_desc(uri.clone(), &doc.text, doc.tree.root_node())
    }

    fn process_includes(
        &mut self,
        project_dir: &Path,
        desc: &ModuleDesc,
        visited: &mut FxHashSet<PathBuf>,
        report: &mut LoadReport,
    ) {
        for inc in &desc.includes {
            // P15.x — runtime rejects absolute @include paths; mirror
            // that here so we don't analyze files that won't actually
            // be loaded at runtime. The `absolute-include` warning is
            // surfaced by `core::diagnostics::pragma_diagnostics`.
            if Path::new(&inc.value).is_absolute() {
                continue;
            }
            let dir = project_dir.join(&inc.value);
            if !self.ctx.is_dir(&dir) {
                // P15.5 — surfaced as a typed `unresolved-include`
                // diagnostic via `core::diagnostics::pragma_diagnostics`.
                // Leave the loader silent so consumers don't see it twice.
                continue;
            }
            for path in self.ctx.iter_gcl(&dir) {
                if let Some(uri) = self.load_file(&path, "project", visited, report) {
                    let nested = self.module_desc_for(&uri);
                    self.process_includes(project_dir, &nested, visited, report);
                    self.process_libraries(project_dir, &nested, visited, report);
                }
            }
        }
    }

    fn process_libraries(
        &mut self,
        project_dir: &Path,
        desc: &ModuleDesc,
        visited: &mut FxHashSet<PathBuf>,
        report: &mut LoadReport,
    ) {
        for lib in &desc.libraries {
            // Local library wins (`<project_dir>/lib/<name>/`). Global `std`
            // falls back to `<greycat_home>/lib/std/`. Other libs without a
            // local copy are reported as unresolved.
            let local = library_dir(project_dir, &lib.name);
            let lib_root = if self.ctx.is_dir(&local) {
                local
            } else if lib.name == "std" {
                let global = global_std_dir(self.ctx.greycat_home());
                if self.ctx.is_dir(&global) {
                    global
                } else {
                    report.unresolved_libraries.push(lib.name.clone());
                    continue;
                }
            } else {
                report.unresolved_libraries.push(lib.name.clone());
                continue;
            };

            for path in self.ctx.iter_gcl(&lib_root) {
                self.load_file(&path, &lib.name, visited, report);
            }
        }
    }

    fn load_file(
        &mut self,
        path: &Path,
        lib: &str,
        visited: &mut FxHashSet<PathBuf>,
        report: &mut LoadReport,
    ) -> Option<Uri> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !visited.insert(canonical.clone()) {
            return None; // already loaded in this call — cycle-safe
        }
        let uri = path_to_uri(&canonical);
        // **P19.22** — idempotent across `load_project` calls. If the
        // document is already in the manager (because the LSP did_open
        // landed first, or a prior `load_project` populated it), skip
        // the disk read so we don't clobber unsaved in-editor edits.
        // Closed-and-externally-edited files are refreshed via the
        // explicit `did_change_watched_files` path, not here.
        // **P19.23** — record in `reachable` regardless of whether we
        // re-read or skipped, so eviction can compute the full closure.
        if self.documents.contains_key(&uri) {
            report.reachable.push(uri.clone());
            return Some(uri);
        }
        // P14.5: split the read and parse phases so cli `lint --csv`
        // can surface them separately. `add_simple` triggers the
        // tree-sitter parse internally; bracketing it captures the
        // parse-only duration.
        let read_start = Instant::now();
        let text = match self.ctx.read(&canonical) {
            Ok(t) => t,
            Err(e) => {
                report
                    .errors
                    .push(format!("cannot read {}: {e}", canonical.display()));
                return None;
            }
        };
        let read = read_start.elapsed();
        let parse_start = Instant::now();
        self.add_simple(uri.clone(), text, lib, false);
        let parse = parse_start.elapsed();
        report
            .loaded
            .push((uri.clone(), LoadTimings { read, parse }));
        report.reachable.push(uri.clone());
        Some(uri)
    }

    // P19.23
    /// Drop every document NOT in `reachable` (and not
    /// currently opened by the editor). Returns the URIs evicted, so
    /// the LSP layer can publish empty diagnostics for them.
    ///
    /// Opened documents are preserved unconditionally: the editor owns
    /// their live state, and a transient pragma edit shouldn't yank a
    /// buffer the user is typing into. They'll fall out naturally on
    /// `did_close` or when the user removes the pragma AND closes the
    /// buffer.
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a HashMap/Set key in practice.
    pub fn evict_unreachable(&mut self, reachable: &FxHashSet<Uri>) -> Vec<Uri> {
        let to_remove: Vec<Uri> = self
            .documents
            .iter()
            .filter_map(|(uri, cell)| {
                if reachable.contains(uri) {
                    return None;
                }
                if cell.borrow().opened {
                    return None;
                }
                Some(uri.clone())
            })
            .collect();
        for uri in &to_remove {
            self.documents.remove(uri);
        }
        to_remove
    }
}

impl Default for SourceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SourceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "SourceManager({}):", self.documents.len())?;
        let last_i = self.documents.len().saturating_sub(1);
        for (i, doc) in self.documents.values().enumerate() {
            let doc = doc.borrow();
            write!(f, "{doc}")?;
            if i < last_i {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

// P33.1
/// Which `std` source the resolver pulled in.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StdResolution {
    /// `<project_dir>/lib/std/` — what `greycat install` populates.
    Local,
    /// `<greycat_home>/lib/std/` — what a host-wide `greycat` install
    /// ships.
    Global,
    /// Neither location had std. The analyzer will produce many
    /// spurious "unresolved type" errors; the LSP / CLI surface this
    /// as a `missing-std` diagnostic on the project.gcl entrypoint.
    #[default]
    Missing,
}

/// Outcome of [`SourceManager::load_project`].
#[derive(Debug, Default, Clone)]
pub struct LoadReport {
    // P14.5 — restored and enriched the per-file timing.
    /// URIs of every document loaded during the call (excluding ones
    /// that were already in the manager) paired with [`LoadTimings`]
    /// — file-read + tree-sitter-parse durations measured separately.
    pub loaded: Vec<(Uri, LoadTimings)>,
    // P19.23 — reachable set. P19.22 — idempotency change.
    /// Every URI reachable from the entrypoint through
    /// `@library` / `@include` traversal, *including* files that were
    /// already in the manager (and therefore absent from `loaded`).
    /// Consumers that need to evict no-longer-reachable docs (LSP
    /// `reload_project_closure`) take the difference between
    /// `manager.iter()` and this set.
    pub reachable: Vec<Uri>,
    // P1.4 — surface as diagnostics.
    /// `@library` declarations that couldn't be resolved to a directory.
    pub unresolved_libraries: Vec<String>,
    // P1.4 — typed diagnostics arrive here.
    /// Filesystem / decoding errors encountered along the way. Strings
    /// for now.
    pub errors: Vec<String>,
    // P33.1
    /// Where (if anywhere) `std` was loaded from. Drives the
    /// `missing-std` entrypoint diagnostic.
    pub std_resolution: StdResolution,
    // P33.1
    /// URI of the project's `project.gcl` entrypoint — `None` only
    /// when the entrypoint itself failed to load.
    pub entrypoint_uri: Option<Uri>,
}

// P14.5
/// Per-file load-phase timings.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoadTimings {
    /// `Context::read` — file I/O / decoding.
    pub read: Duration,
    /// `Document::with_lib` — tree-sitter parse (`syntax::parse`).
    pub parse: Duration,
}

impl LoadTimings {
    pub fn total(&self) -> Duration {
        self.read + self.parse
    }
}

impl LoadReport {
    /// Iterator over the loaded URIs (compat shim for callers that
    /// don't need the per-file timing).
    pub fn loaded_uris(&self) -> impl Iterator<Item = &Uri> {
        self.loaded.iter().map(|(u, _)| u)
    }
}

/// Build a `file://` [`Uri`] from a filesystem path. The single
/// source of truth for path → URI formatting so every producer
/// (loader, LSP watcher, tests) yields byte-identical URIs — keys in
/// `uri_owner` / the manager's document map are compared verbatim, so
/// a divergent encoding would silently fail to match a loaded file.
/// Callers that need the URI to match a *loaded* document must pass an
/// already-canonicalized path (the loader canonicalizes before calling
/// this).
pub fn path_to_uri(path: &Path) -> Uri {
    let s = format!("file://{}", path.display());
    s.parse::<Uri>()
        .unwrap_or_else(|_| "file:///invalid".parse().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::Arc;

    /// In-memory `Context` for tests — keeps these hermetic, no temp dirs,
    /// no `$HOME` / `$GREYCAT_HOME` mutation.
    #[derive(Default)]
    struct MemContext {
        files: std::sync::Mutex<FxHashMap<PathBuf, String>>,
        dirs: std::sync::Mutex<FxHashSet<PathBuf>>,
        greycat_home: PathBuf,
    }

    impl MemContext {
        fn add_file(&self, path: PathBuf, content: &str) {
            // Mark every ancestor as a directory so is_dir / iter_gcl
            // walking matches a real filesystem.
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
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string())
                })
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

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).unwrap()
    }

    #[test]
    fn add_simple_round_trip() {
        let ctx = Arc::new(MemContext::default());
        let mut mgr = SourceManager::with_context(ctx);
        mgr.add_simple(uri("/proj/src/a.gcl"), "fn a() {}\n", "project", false);
        assert_eq!(mgr.len(), 1);
        let cell = mgr.get(&uri("/proj/src/a.gcl")).unwrap();
        let doc = cell.borrow();
        assert_eq!(doc.lib, "project");
        assert!(!doc.opened);
        assert_eq!(doc.root_node().kind(), "module");
    }

    #[test]
    fn load_project_walks_includes_and_local_lib() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(
            PathBuf::from("/proj/project.gcl"),
            "@library(\"std\", \"1.0\");\n@include(\"src\");\n",
        );
        ctx.add_file(PathBuf::from("/proj/src/main.gcl"), "fn main() {}\n");
        ctx.add_file(PathBuf::from("/proj/src/util.gcl"), "fn util() {}\n");
        ctx.add_file(PathBuf::from("/proj/lib/std/core.gcl"), "fn core() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));

        // 1 project.gcl + 2 include files + 1 lib file
        assert_eq!(report.loaded.len(), 4, "loaded: {:?}", report.loaded);
        assert!(report.unresolved_libraries.is_empty());
        assert!(report.errors.is_empty());

        let main_uri = uri("/proj/src/main.gcl");
        let main = mgr.get(&main_uri).unwrap().borrow();
        assert_eq!(main.lib, "project");

        let core_uri = uri("/proj/lib/std/core.gcl");
        let core_doc = mgr.get(&core_uri).unwrap().borrow();
        assert_eq!(core_doc.lib, "std");
    }

    #[test]
    fn load_project_global_std_fallback() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(
            PathBuf::from("/proj/project.gcl"),
            "@library(\"std\", \"1.0\");\n",
        );
        ctx.add_file(PathBuf::from("/gcat/lib/std/core.gcl"), "fn core() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.loaded.len(), 2, "loaded: {:?}", report.loaded);
        assert!(report.unresolved_libraries.is_empty());
        assert_eq!(
            mgr.get(&uri("/gcat/lib/std/core.gcl"))
                .unwrap()
                .borrow()
                .lib,
            "std"
        );
    }

    #[test]
    fn load_project_unresolved_library_reported() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(
            PathBuf::from("/proj/project.gcl"),
            "@library(\"missing\", \"1.0\");\n",
        );

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.loaded.len(), 1); // just the project.gcl
        assert_eq!(report.unresolved_libraries, vec!["missing".to_string()]);
    }

    // P19.22
    /// `load_project` must be idempotent across calls so
    /// the LSP can re-walk pragmas after an in-editor edit without
    /// clobbering unsaved buffers (the editor owns the live state via
    /// `did_change`). Concretely: pre-populate the entrypoint with a
    /// fresh in-memory text, then call `load_project` against a
    /// *different* on-disk version; the in-memory text must survive
    /// and only the newly-referenced files should be loaded.
    #[test]
    fn load_project_preserves_in_memory_documents() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        // On-disk version has no `@include`. The in-memory edit
        // we'll seed below adds one — this simulates the user
        // typing `@include("src");` in the editor.
        ctx.add_file(PathBuf::from("/proj/project.gcl"), "fn main() {}\n");
        ctx.add_file(PathBuf::from("/proj/src/a.gcl"), "fn a() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        // Seed the manager with the in-editor (newer) content. `opened
        // = true` mirrors what `Backend::did_open` does.
        mgr.add_simple(
            uri("/proj/project.gcl"),
            "@include(\"src\");\nfn main() {}\n",
            "project",
            true,
        );

        let report = mgr.load_project(Path::new("/proj/project.gcl"));

        // Only `src/a.gcl` should appear in `loaded` — project.gcl was
        // already in the manager and must NOT be re-read from disk.
        assert_eq!(
            report.loaded.len(),
            1,
            "expected only src/a.gcl as new; got: {:?}",
            report.loaded
        );
        assert_eq!(report.loaded[0].0.as_str(), "file:///proj/src/a.gcl");

        // The in-editor version must survive the re-walk.
        let entry = mgr.get(&uri("/proj/project.gcl")).unwrap().borrow();
        assert!(entry.opened, "opened flag must be preserved");
        assert!(
            entry.text.contains("@include"),
            "in-editor pragma must survive: text={:?}",
            entry.text
        );
    }

    // P33.1
    /// Even without `@library("std", ...)` in project.gcl, the
    /// resolver pulls in the local `lib/std` when it exists. Mirrors
    /// the runtime's behavior of preferring a project-local std over
    /// the embedded one.
    #[test]
    fn load_project_auto_loads_local_std_without_declaration() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(PathBuf::from("/proj/project.gcl"), "fn main() {}\n");
        ctx.add_file(PathBuf::from("/proj/lib/std/core.gcl"), "fn core() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.std_resolution, StdResolution::Local);
        assert_eq!(report.loaded.len(), 2, "loaded: {:?}", report.loaded);
        let core = mgr
            .get(&uri("/proj/lib/std/core.gcl"))
            .expect("core.gcl loaded");
        assert_eq!(core.borrow().lib, "std");
    }

    // P33.1
    /// No local `lib/std` — the resolver falls back to
    /// `<greycat_home>/lib/std/` even without an `@library("std")`
    /// declaration.
    #[test]
    fn load_project_auto_loads_global_std_without_declaration() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(PathBuf::from("/proj/project.gcl"), "fn main() {}\n");
        ctx.add_file(PathBuf::from("/gcat/lib/std/core.gcl"), "fn core() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.std_resolution, StdResolution::Global);
        assert_eq!(report.loaded.len(), 2);
        let core = mgr
            .get(&uri("/gcat/lib/std/core.gcl"))
            .expect("global core.gcl loaded");
        assert_eq!(core.borrow().lib, "std");
    }

    // P33.1
    /// No std anywhere — the report records `StdResolution::Missing`
    /// so the LSP / CLI can emit a `missing-std` advisory on the
    /// entrypoint.
    #[test]
    fn load_project_reports_missing_std() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        ctx.add_file(PathBuf::from("/proj/project.gcl"), "fn main() {}\n");

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.std_resolution, StdResolution::Missing);
        // Just the entrypoint loaded.
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(
            report.entrypoint_uri.as_ref().map(|u| u.as_str()),
            Some("file:///proj/project.gcl")
        );
    }

    #[test]
    fn load_project_cycle_safe() {
        let ctx = MemContext {
            greycat_home: PathBuf::from("/gcat"),
            ..Default::default()
        };
        // Two files that both `@include("src")` — the second walk through
        // `src/` should be a no-op because file paths are already visited.
        ctx.add_file(PathBuf::from("/proj/project.gcl"), "@include(\"src\");\n");
        ctx.add_file(
            PathBuf::from("/proj/src/a.gcl"),
            "@include(\"src\");\nfn a() {}\n",
        );

        let mut mgr = SourceManager::with_context(Arc::new(ctx));
        let report = mgr.load_project(Path::new("/proj/project.gcl"));
        assert_eq!(report.loaded.len(), 2);
        assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    }
}
