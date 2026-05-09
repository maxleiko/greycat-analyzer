//! P19.23 — when the user comments out / removes an `@library`
//! pragma from `project.gcl`, the LSP must evict the library's
//! modules from the manager and rebuild the analysis. Otherwise the
//! types the lib exposes still resolve, masking the resulting
//! "unknown type" error.
//!
//! Test shape:
//! 1. Set up a project with `@library("foo", "1.0")` where `lib/foo/`
//!    declares a `Foo` type. `main.gcl` uses `Foo` as a fn return type.
//! 2. Run `ProjectAnalysis::analyze` — expect zero errors (`Foo` resolves).
//! 3. Replace `project.gcl`'s in-memory text to comment out the `@library`
//!    line. Re-walk via `load_project` (the LSP's `reload_project_closure`
//!    path), evict unreachable docs, rebuild.
//! 4. Expect an unresolved-name diagnostic on the `Foo` reference in
//!    `main.gcl`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::resolver::Context;

/// In-memory `Context` for hermetic tests. `Mutex` keeps the `Arc<dyn
/// Context>` Send+Sync without an unsafe impl; tests are single-
/// threaded so the lock is uncontended.
#[derive(Default)]
struct MemContext {
    files: Mutex<HashMap<PathBuf, String>>,
    dirs: Mutex<HashSet<PathBuf>>,
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

fn uri_for(path: &str) -> Uri {
    use std::str::FromStr;
    Uri::from_str(&format!("file://{path}")).unwrap()
}

#[test]
fn commenting_out_library_evicts_modules_and_surfaces_unresolved_type() {
    // 1. Set up project + lib + main.
    let ctx = MemContext::default();
    ctx.add_file(
        PathBuf::from("/proj/project.gcl"),
        "@library(\"foo\", \"1.0\");\n@include(\"src\");\n",
    );
    ctx.add_file(
        PathBuf::from("/proj/src/main.gcl"),
        "fn use_foo(): Foo { return Foo {}; }\n",
    );
    ctx.add_file(PathBuf::from("/proj/lib/foo/types.gcl"), "type Foo {}\n");

    let mut mgr = SourceManager::with_context(Arc::new(ctx));
    let report = mgr.load_project(Path::new("/proj/project.gcl"));
    assert!(report.errors.is_empty(), "load errors: {:?}", report.errors);
    assert_eq!(
        report.loaded.len(),
        3,
        "expected 3 files loaded (project + main + lib type), got {:?}",
        report.loaded
    );

    let mut analysis = ProjectAnalysis::default();
    analysis.rebuild(&mgr);

    // 2. Initial state: `Foo` resolves, no unresolved-name diagnostic.
    let main_uri = uri_for("/proj/src/main.gcl");
    let initial = analysis.module(&main_uri).expect("main module analyzed");
    let unresolved_initial: Vec<_> = initial
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("unresolved") || d.message.contains("unknown"))
        .collect();
    assert!(
        unresolved_initial.is_empty(),
        "expected `Foo` to resolve initially, got: {:?}",
        unresolved_initial
    );

    // 3. Simulate the user commenting out the `@library` line in the
    //    editor. The LSP would have already updated `project.gcl`'s
    //    in-memory text via `did_change`; mirror that here.
    {
        let cell = mgr.get(&uri_for("/proj/project.gcl")).unwrap();
        let mut doc = cell.borrow_mut();
        doc.text = "// @library(\"foo\", \"1.0\");\n@include(\"src\");\n".into();
        doc.tree = greycat_analyzer_syntax::parse(&doc.text);
    }

    // 4. Re-walk pragmas (idempotent — won't clobber the in-memory
    //    project.gcl), then evict anything no longer reachable.
    let report = mgr.load_project(Path::new("/proj/project.gcl"));
    #[allow(clippy::mutable_key_type)] // lsp_types::Uri as a HashSet key is fine in practice.
    let reachable: HashSet<Uri> = report.reachable.iter().cloned().collect();
    let evicted = mgr.evict_unreachable(&reachable);
    assert!(
        evicted
            .iter()
            .any(|u| u.as_str().contains("/lib/foo/types.gcl")),
        "expected lib/foo/types.gcl to be evicted, got: {:?}",
        evicted
    );

    // 5. Rebuild — `Foo` should now be unresolved.
    analysis.rebuild(&mgr);
    let final_mod = analysis.module(&main_uri).expect("main module re-analyzed");
    let unresolved_final: Vec<_> = final_mod
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("Foo"))
        .collect();
    assert!(
        !unresolved_final.is_empty(),
        "expected an unresolved-`Foo` diagnostic after lib removal, got: {:?}",
        final_mod.analysis.diagnostics
    );
}
