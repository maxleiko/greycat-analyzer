//! Regression test for `fix(analysis): private fn module::name typed
//! as function, not type`.
//!
//! Before the fix, a 2-segment `module::name` chain typed the
//! reference as `type` whenever `name` was a non-native fn without a
//! declared return type (private or not). `fn_signatures` skips such
//! fns at signature-lowering time, but they're still in `index.values`,
//! and the runtime treats `module::name` as a function ref regardless.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn analyze_two_modules(helper_src: &str, main_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/helper.gcl", helper_src);
    let main_uri = add(&mut mgr, "/proj/src/main.gcl", main_src);
    (main_uri, ProjectAnalysis::analyze(&mgr))
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn private_fn_without_return_type_resolves_as_function_ref() {
    // `helper::my_fn` where `my_fn` is `private fn my_fn() {}` —
    // accepted as a `function` argument at runtime. Before the fix,
    // the analyzer surfaced "value of type `type` is not assignable
    // to parameter `f: function`".
    let helper = "private fn my_fn() {}\n";
    let main = "\
fn takesFn(f: function) {}
fn caller() {
    takesFn(helper::my_fn);
}
";
    let (uri, pa) = analyze_two_modules(helper, main);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn fn_without_return_type_resolves_as_function_ref() {
    // Same as above but `fn` (not `private`). Both shapes skip
    // `fn_signatures` (no return type), so both must take the new
    // `contains_value` branch.
    let helper = "fn my_fn() {}\n";
    let main = "\
fn takesFn(f: function) {}
fn caller() {
    takesFn(helper::my_fn);
}
";
    let (uri, pa) = analyze_two_modules(helper, main);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn fn_with_return_type_still_resolves_as_function_ref() {
    // Regression guard: fns *with* a declared return type still
    // resolve via `fn_signatures` (the original `function` branch),
    // not the new `contains_value` fallback.
    let helper = "fn my_fn(): int { return 1; }\n";
    let main = "\
fn takesFn(f: function) {}
fn caller() {
    takesFn(helper::my_fn);
}
";
    let (uri, pa) = analyze_two_modules(helper, main);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn module_qualified_type_still_resolves_as_type() {
    // Negative: `module::TypeName` (where TypeName is a type, not a
    // fn) must keep resolving as `type`. The new branch sits between
    // `contains_fn_signature` and the type / has_name fallback, so
    // type references must still flow through correctly.
    let helper = "type MyType {}\n";
    let main = "\
fn takesType(t: type) {}
fn caller() {
    takesType(helper::MyType);
}
";
    let (uri, pa) = analyze_two_modules(helper, main);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}
