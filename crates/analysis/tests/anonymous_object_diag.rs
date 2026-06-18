//! Regression test for Bug 1 — anonymous object expressions.
//!
//! GreyCat has no anonymous objects: the type identifier before `{` is
//! mandatory. A missing head parses as an empty-symbol `TypeRef`, which
//! previously surfaced the misleading `unresolved name `` `. Now it
//! raises a single, well-spanned `anonymous-object` error instead.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///proj/src/main.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri.clone(), ProjectAnalysis::analyze(&mgr))
}

fn codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn named_anonymous_object_flags_anonymous_object_not_unresolved_name() {
    let (uri, pa) = analyze("fn f() {\n    var o = { foo: 42 };\n}\n");
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "anonymous-object"),
        "expected an `anonymous-object` diagnostic, got: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "unresolved-name"),
        "the misleading empty-name `unresolved-name` must be suppressed, got: {codes:?}"
    );
}

#[test]
fn positional_anonymous_object_flags_anonymous_object() {
    let (uri, pa) = analyze("fn f() {\n    var o = { 42 };\n}\n");
    let codes = codes(&pa, &uri);
    assert!(
        codes.iter().any(|c| c == "anonymous-object"),
        "expected an `anonymous-object` diagnostic, got: {codes:?}"
    );
    assert!(
        !codes.iter().any(|c| c == "unresolved-name"),
        "the misleading empty-name `unresolved-name` must be suppressed, got: {codes:?}"
    );
}

#[test]
fn named_object_with_type_head_is_not_flagged() {
    // Guard: a real `Foo { ... }` must never trip the anonymous check.
    let (uri, pa) =
        analyze("type Foo {\n    foo: int;\n}\nfn f() {\n    var o = Foo { foo: 42 };\n}\n");
    let codes = codes(&pa, &uri);
    assert!(
        !codes.iter().any(|c| c == "anonymous-object"),
        "a typed object head must not be flagged, got: {codes:?}"
    );
}
