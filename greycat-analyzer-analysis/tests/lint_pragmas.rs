// P40.1 — project-pragma lint control.
//
// `@lint_on("rule")` at the project entrypoint surfaces a default-off
// rule project-wide; `@lint_off("rule")` silences a rule project-wide.
// Per-module pragmas only apply to that module.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHashSet};

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::resolver::Context;

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

/// Build a two-module project: `entrypoint_text` lands in
/// `/proj/project.gcl` (with `@include("src")` prepended), `other_text`
/// in `/proj/src/other.gcl`. Returns `(entry_uri, other_uri, analysis)`.
fn analyze_two_modules(entrypoint_text: &str, other_text: &str) -> (Uri, Uri, ProjectAnalysis) {
    let ctx = MemContext::default();
    ctx.add_file(
        PathBuf::from("/proj/project.gcl"),
        &format!("@include(\"src\");\n{entrypoint_text}"),
    );
    ctx.add_file(PathBuf::from("/proj/src/other.gcl"), other_text);

    let mut mgr = SourceManager::with_context(Arc::new(ctx));
    let report = mgr.load_project(Path::new("/proj/project.gcl"));
    assert!(report.errors.is_empty(), "load errors: {:?}", report.errors);

    let pa = ProjectAnalysis::analyze(&mgr);
    (
        uri_for("/proj/project.gcl"),
        uri_for("/proj/src/other.gcl"),
        pa,
    )
}

fn lints_for_rule(pa: &ProjectAnalysis, uri: &Uri, rule: &str) -> usize {
    pa.module(uri)
        .unwrap()
        .lints
        .iter()
        .filter(|l| l.rule == rule)
        .count()
}

#[test]
fn entrypoint_lint_on_enables_default_off_rule_project_wide() {
    let entry = "@lint_on(\"no-breakpoint\");\nfn entry() { breakpoint; }\n";
    let other = "fn other() { breakpoint; }\n";
    let (entry_uri, other_uri, pa) = analyze_two_modules(entry, other);
    assert_eq!(
        lints_for_rule(&pa, &entry_uri, "no-breakpoint"),
        1,
        "entrypoint @lint_on should surface no-breakpoint on the entrypoint: {:?}",
        pa.module(&entry_uri).unwrap().lints
    );
    assert_eq!(
        lints_for_rule(&pa, &other_uri, "no-breakpoint"),
        1,
        "entrypoint @lint_on should surface no-breakpoint on other modules too: {:?}",
        pa.module(&other_uri).unwrap().lints
    );
}

#[test]
fn module_lint_on_only_affects_that_module() {
    let entry = "fn entry() { breakpoint; }\n";
    let other = "@lint_on(\"no-breakpoint\");\nfn other() { breakpoint; }\n";
    let (entry_uri, other_uri, pa) = analyze_two_modules(entry, other);
    assert_eq!(
        lints_for_rule(&pa, &entry_uri, "no-breakpoint"),
        0,
        "entrypoint without @lint_on must not surface no-breakpoint: {:?}",
        pa.module(&entry_uri).unwrap().lints
    );
    assert_eq!(
        lints_for_rule(&pa, &other_uri, "no-breakpoint"),
        1,
        "module-scope @lint_on should surface no-breakpoint there: {:?}",
        pa.module(&other_uri).unwrap().lints
    );
}

#[test]
fn entrypoint_lint_off_silences_project_wide() {
    let entry = "@lint_off(\"unused-decl\");\nprivate fn entry_unused() {}\n";
    let other = "private fn other_unused() {}\n";
    let (entry_uri, other_uri, pa) = analyze_two_modules(entry, other);
    assert_eq!(
        lints_for_rule(&pa, &entry_uri, "unused-decl"),
        0,
        "entrypoint @lint_off should silence on the entrypoint: {:?}",
        pa.module(&entry_uri).unwrap().lints
    );
    assert_eq!(
        lints_for_rule(&pa, &other_uri, "unused-decl"),
        0,
        "entrypoint @lint_off should silence other modules too: {:?}",
        pa.module(&other_uri).unwrap().lints
    );
}

#[test]
fn module_lint_off_only_affects_that_module() {
    let entry = "private fn entry_unused() {}\n";
    let other = "@lint_off(\"unused-decl\");\nprivate fn other_unused() {}\n";
    let (entry_uri, other_uri, pa) = analyze_two_modules(entry, other);
    assert_eq!(
        lints_for_rule(&pa, &entry_uri, "unused-decl"),
        1,
        "entrypoint should still surface unused-decl: {:?}",
        pa.module(&entry_uri).unwrap().lints
    );
    assert_eq!(
        lints_for_rule(&pa, &other_uri, "unused-decl"),
        0,
        "module-scope @lint_off should silence locally: {:?}",
        pa.module(&other_uri).unwrap().lints
    );
}
