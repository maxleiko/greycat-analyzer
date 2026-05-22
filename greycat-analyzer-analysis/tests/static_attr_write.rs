//! Assignment to a `static` attribute through a `Type::name`
//! (or `module::Type::name`) path expression is a hard semantic
//! error — the runtime does not allow it, and there is no scope in
//! which it would be allowed (unlike `private`, where the type's
//! constructor can write).

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn direct_static_assignment_errors_same_module() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Counter {\n    static count: int = 0;\n}\n\
         fn bump() { Counter::count = 42; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "static-attr-assign"),
        "expected `static-attr-assign` on `Counter::count = 42`; got: {codes:?}",
    );
}

#[test]
fn coalesce_static_assignment_errors_same_module() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Counter {\n    static count: int = 0;\n}\n\
         fn bump() { Counter::count ?= 42; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "static-attr-assign"),
        "expected `static-attr-assign` on `Counter::count ?= 42`; got: {codes:?}",
    );
}

#[test]
fn cross_module_static_assignment_errors() {
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/counter.gcl",
        "type Counter {\n    static count: int = 0;\n}\n",
    );
    let user_uri = add(
        &mut mgr,
        "/proj/user.gcl",
        "fn bump() { Counter::count = 1; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &user_uri);
    assert!(
        codes.iter().any(|c| c == "static-attr-assign"),
        "expected `static-attr-assign` cross-module too; got: {codes:?}",
    );
}

#[test]
fn read_of_static_attr_is_allowed() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Counter {\n    static count: int = 0;\n}\n\
         fn show(): int { return Counter::count; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        !codes.iter().any(|c| c == "static-attr-assign"),
        "read of static attr must not emit static-attr-assign; got: {codes:?}",
    );
}

#[test]
fn non_static_field_handle_is_unaffected() {
    // `Type::field_name` where `field_name` is an *instance* attr
    // (not `static`) yields a `field` handle, not a writable lvalue.
    // The new check must NOT fire on this shape — it's reserved for
    // the static-attr case.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type User {\n    name: String;\n}\n\
         fn handle(): field { return User::name; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let codes = codes(&pa, &uri);
    assert!(
        !codes.iter().any(|c| c == "static-attr-assign"),
        "field-handle read must not emit static-attr-assign; got: {codes:?}",
    );
}
