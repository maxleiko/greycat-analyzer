//! Local-rebinding diagnostics.
//!
//! GreyCat doesn't allow re-binding a name that's already declared as
//! a `Local` or `Param` in the *same* lexical scope. The runtime
//! rejects this shape with `already declared var` / `already declared
//! param`; the analyzer surfaces it as `local-rebind`. Nested-scope
//! shadowing is still allowed.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (ProjectAnalysis, Uri) {
    let uri = Uri::from_str("file:///local_rebind.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    (pa, uri)
}

fn rebind_diagnostics<'a>(pa: &'a ProjectAnalysis, uri: &Uri) -> Vec<&'a str> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.code == "local-rebind")
        .map(|d| d.message.as_str())
        .collect()
}

#[test]
fn var_rebinding_param_is_error() {
    let src = "\
fn foo(x: int) {
    var x = 42;
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
    assert!(diags[0].contains('x'), "got: {}", diags[0]);
}

#[test]
fn two_vars_same_block_is_error() {
    let src = "\
fn foo() {
    var x = 1;
    var x = 2;
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn duplicate_params_is_error() {
    let src = "\
fn foo(x: int, x: String) {
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn nested_block_shadow_is_allowed() {
    let src = "\
fn foo(x: int) {
    if (x > 0) {
        var x = 42;
    }
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert!(diags.is_empty(), "expected no rebind diags, got: {diags:?}");
}

#[test]
fn for_loop_body_var_shadow_is_allowed() {
    let src = "\
fn foo() {
    for (var i = 0; i < 3; i = i + 1) {
        var i = 99;
    }
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert!(diags.is_empty(), "got: {diags:?}");
}

#[test]
fn lambda_param_rebind_in_body_is_error() {
    let src = "\
fn foo() {
    var f = fn(a: int): int {
        var a = 99;
        return a;
    };
    f(1);
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn lambda_param_shadowing_outer_local_is_allowed() {
    // Lambdas have a closed scope — their params don't collide with
    // names in the enclosing function.
    let src = "\
fn foo() {
    var x = 1;
    var f = fn(x: int): int { return x; };
    f(x);
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert!(diags.is_empty(), "got: {diags:?}");
}

#[test]
fn catch_param_rebind_in_catch_body_is_error() {
    let src = "\
fn foo() {
    try {
        throw \"oops\";
    } catch (e) {
        var e = 99;
    }
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn duplicate_for_in_params_is_error() {
    let src = "\
fn foo() {
    var m = Map<String, int>{};
    for (k, k in m) {
    }
}
";
    let (pa, uri) = analyze(src);
    let diags = rebind_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}
