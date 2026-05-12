//! P38.4 — `ambiguous-symbol` Severity::Error when ≥2 public modules
//! export the same name and the current module has no local hit.
//! Matches the GreyCat runtime (8.0.291-dev) outcome on probes p7
//! and p10: bare reference is unresolved (exit 2). We surface the
//! same Error severity but name the candidate modules so users can
//! pick an FQN.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_analysis::resolver::Definition;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn ambiguous_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.message.contains("ambiguous-symbol"))
        .map(|d| d.message.clone())
        .collect()
}

fn unresolved_name_count(pa: &ProjectAnalysis, uri: &Uri) -> usize {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.starts_with("unresolved name"))
        .count()
}

#[test]
fn ambiguous_when_two_modules_export_same_public_name() {
    // p7 mirror: bare `greeting()` from project.gcl (no local hit) +
    // two sibling modules each `fn greeting()` public.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/a.gcl", "fn greeting() {}\n");
    add(&mut mgr, "/proj/src/b.gcl", "fn greeting() {}\n");
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {
    greeting();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = ambiguous_diagnostics(&pa, &main_uri);
    assert_eq!(
        diags.len(),
        1,
        "expected one ambiguous-symbol error, got: {:#?}",
        diags
    );
    assert!(diags[0].contains("greeting"));
    assert!(diags[0].contains("`a::greeting`") || diags[0].contains("`b::greeting`"));
    assert!(diags[0].contains("a"));
    assert!(diags[0].contains("b"));
    // P38.4 — duplicate "unresolved name" must NOT also fire for the
    // same ident; the ambiguous-symbol diagnostic supersedes it.
    assert_eq!(
        unresolved_name_count(&pa, &main_uri),
        0,
        "ambiguous ident must not also surface as `unresolved name`"
    );
}

#[test]
fn no_ambiguous_when_one_module_has_local_match() {
    // p8 mirror: local public wins, no ambiguity even though two
    // modules export the same name.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/a.gcl", "fn greeting() {}\n");
    let local_uri = add(
        &mut mgr,
        "/proj/src/b.gcl",
        "fn greeting() {}
fn caller() {
    greeting();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = ambiguous_diagnostics(&pa, &local_uri);
    assert!(
        diags.is_empty(),
        "expected no ambiguous-symbol (local public shadows): {:#?}",
        diags
    );
}

#[test]
fn no_ambiguous_when_extra_module_has_private() {
    // Mirror of the visibility-filter rule: a private same-named decl
    // in another module is invisible to bare lookup, so it doesn't
    // count toward the cross-module clash. With one public + one
    // private, the public wins cleanly.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/a.gcl", "fn greeting() {}\n");
    add(&mut mgr, "/proj/src/b.gcl", "private fn greeting() {}\n");
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller() {
    greeting();
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = ambiguous_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no ambiguous-symbol when extra hit is private: {:#?}",
        diags
    );
    // The bare ref should resolve to `a.gcl`'s public, not be
    // ambiguous or unresolved.
    let m = pa.module(&main_uri).unwrap();
    let resolved: Vec<_> = m
        .resolutions
        .uses
        .values()
        .filter(|d| matches!(d, Definition::ProjectDecl { .. }))
        .collect();
    assert!(
        !resolved.is_empty(),
        "expected at least one ProjectDecl binding"
    );
}
