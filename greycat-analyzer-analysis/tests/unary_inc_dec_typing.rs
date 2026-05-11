//! Regression test for `fix(hir): lower ++ / -- / + as Inc/Dec/Pos`:
//! the analyzer must type `++c.x` (and `c.x++`, `--c.x`, etc.) as the
//! operand's type (here `int`), not as `bool` (which is what the old
//! wildcard-to-`Not` lowering produced).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable to parameter"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn prefix_inc_on_int_field_passed_to_int_param_is_accepted() {
    // `++c.x` should type as `int` (the field's type), so the call
    // resolves cleanly. The pre-fix analyzer typed this as `bool`.
    let src = "\
type Counter { x: int; }
fn takesInt(v: int) {}
fn main() {
    var c = Counter { x: 0 };
    takesInt(++c.x);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {diags:?}"
    );
}

#[test]
fn postfix_inc_on_float_field_passed_to_float_param_is_accepted() {
    let src = "\
type Counter { x: float; }
fn takesFloat(v: float) {}
fn main() {
    var c = Counter { x: 0.0 };
    takesFloat(c.x++);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {diags:?}"
    );
}

#[test]
fn unary_minus_on_int_param_keeps_int_type() {
    let src = "\
fn takesInt(v: int) {}
fn main() {
    var x = 5;
    takesInt(-x);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {diags:?}"
    );
}
