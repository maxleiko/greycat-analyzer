//! Regression test: P19.19's call-arity check (introduced in `a448993`)
//! originally only fired for bare-fn / static / qualified callees. Method
//! calls (`f.method(...)`, `n->method(...)`) escaped the check because
//! `resolve_call_target` didn't cover `Expr::Member` / `Expr::Arrow`.
//!
//! These tests anchor the extended coverage: every callee shape that
//! resolves to a `Decl::Fn` is now arity-checked through the same path.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = add(&mut mgr, "/proj/src/main.gcl", src);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn analyze_two(helper_src: &str, main_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/helper.gcl", helper_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);
    (main_uri, ProjectAnalysis::analyze(&mgr))
}

fn arity_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("expects") && d.message.contains("argument"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn method_call_with_wrong_arity_is_diagnosed() {
    let src = "\
fn bar(f: Foo) {
    f.replace(\"\");
}

type Foo {
    native fn replace(s1: String, s2: String);
}
";
    let (uri, pa) = analyze(src);
    let diags = arity_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one arity diag, got: {diags:?}");
    assert!(
        diags[0].contains("`replace`")
            && diags[0].contains("expects 2")
            && diags[0].contains("got 1"),
        "unexpected diag text: {}",
        diags[0]
    );
}

#[test]
fn method_call_with_matching_arity_is_clean() {
    let src = "\
fn bar(f: Foo) {
    f.replace(\"a\", \"b\");
}

type Foo {
    native fn replace(s1: String, s2: String);
}
";
    let (uri, pa) = analyze(src);
    let diags = arity_diagnostics(&pa, &uri);
    assert!(diags.is_empty(), "expected no arity diags, got: {diags:?}");
}

#[test]
fn cross_module_method_call_is_arity_checked() {
    let helper = "\
type Foo {
    native fn replace(s1: String, s2: String);
}
";
    let main = "\
fn bar(f: helper::Foo) {
    f.replace(\"\");
}
";
    let (uri, pa) = analyze_two(helper, main);
    let diags = arity_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one arity diag, got: {diags:?}");
    assert!(
        diags[0].contains("`replace`")
            && diags[0].contains("expects 2")
            && diags[0].contains("got 1"),
        "unexpected diag text: {}",
        diags[0]
    );
}

#[test]
fn static_fn_arity_still_checked() {
    // Sanity guard: the bare-fn arity check from P19.19 didn't regress
    // when the method-call path was added.
    let src = "\
fn bar() {
    replace(\"\");
}

native fn replace(s1: String, s2: String);
";
    let (uri, pa) = analyze(src);
    let diags = arity_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one arity diag, got: {diags:?}");
}
