//! Lambda capture diagnostics.
//!
//! GreyCat lambdas have a *closed* scope — only the lambda's own
//! params + body locals + module-scope decls are reachable. The
//! runtime rejects refs to enclosing-function locals/params with
//! `unresolved identifier`, and segfaults on `this` references. The
//! analyzer surfaces both as the `lambda-capture` error code.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (ProjectAnalysis, Uri) {
    let uri = Uri::from_str("file:///lambda_capture.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    (pa, uri)
}

fn capture_diagnostics<'a>(pa: &'a ProjectAnalysis, uri: &Uri) -> Vec<&'a str> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.code == "lambda-capture")
        .map(|d| d.message.as_str())
        .collect()
}

#[test]
fn capturing_enclosing_local_is_error() {
    let src = "\
fn main() {
    var n = 41;
    var f = fn (): int { return n + 1; };
    println(f());
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one lambda-capture, got: {diags:?}"
    );
    assert!(
        diags[0].contains('n'),
        "diagnostic should name the captured ident, got: {}",
        diags[0]
    );
}

#[test]
fn capturing_enclosing_param_is_error() {
    let src = "\
fn outer(x: int): int {
    var f = fn (): int { return x; };
    return f();
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn capturing_outer_lambda_param_is_error() {
    // The inner lambda's `a` lookup walks past the outer lambda's
    // LambdaBody boundary — that's a capture too, not just the
    // "lambda inside fn" case.
    let src = "\
fn main() {
    var outer = fn (a: int): int {
        var inner = fn (): int { return a + 1; };
        return inner();
    };
    println(outer(5));
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert_eq!(diags.len(), 1, "got: {diags:?}");
}

#[test]
fn this_inside_lambda_is_error() {
    let src = "\
type Counter {
    n: int;

    fn bump(): int {
        var f = fn (): int { return this.n + 1; };
        return f();
    }
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected exactly one `this` capture, got: {diags:?}"
    );
    assert!(
        diags[0].contains("this"),
        "diagnostic should mention `this`, got: {}",
        diags[0]
    );
}

#[test]
fn own_param_is_allowed() {
    // The lambda's own params are reachable from its body — no capture.
    let src = "\
fn main() {
    var f = fn (x: int): int { return x + 1; };
    println(f(41));
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "own param should not trigger lambda-capture; got: {diags:?}"
    );
}

#[test]
fn own_local_is_allowed() {
    // A var declared inside the lambda body — not a capture.
    let src = "\
fn main() {
    var f = fn (): int {
        var m = 41;
        return m + 1;
    };
    println(f());
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert!(diags.is_empty(), "got: {diags:?}");
}

#[test]
fn module_scope_decl_is_allowed() {
    // Module-level fn called from inside the lambda is fine — the
    // runtime accepts this shape.
    let src = "\
fn helper(x: int): int {
    return x * 2;
}

fn main() {
    var f = fn (a: int): int { return helper(a); };
    println(f(5));
}
";
    let (pa, uri) = analyze(src);
    let diags = capture_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "module-scope decl ref should not capture; got: {diags:?}"
    );
}
