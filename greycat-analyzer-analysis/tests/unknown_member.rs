//! `resolve_member_with` (analyzer.rs) emits a hard error when a `.`
//! / `->` / `::` lookup exhausts the attr+method chain on a known
//! receiver type. Receivers typed `any` / `any?` / unresolved /
//! generic-param remain silent by design — `any` is anything.
//!
//! These tests use locally-declared types only (no stdlib loaded via
//! `add_simple`); the analyzer gates emission on
//! `local_type_decl.is_some() || index.type_members_for(name).is_some()`
//! so a test fixture that doesn't know the type's body stays silent.
//! Primitive-receiver coverage lives in the CLI smoke tests where
//! stdlib is loaded normally.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (ProjectAnalysis, Uri) {
    let uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    (pa, uri)
}

fn error_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

/// Unknown member on a local non-generic type — `Foo` declares `x`
/// but the source asks for `nope`.
#[test]
fn unknown_member_on_local_type() {
    let src = "type Foo { x: int; }\nfn f(f: Foo) {\n    f.nope;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter().any(|m| m == "type `Foo` has no member `nope`"),
        "got: {msgs:?}"
    );
}

/// Unknown static member on a local type.
#[test]
fn unknown_static_member_on_local_type() {
    let src = "type Foo { static fn bar() {} }\nfn f() {\n    var _ = Foo::nope;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m == "type `Foo` has no static member `nope`"),
        "got: {msgs:?}"
    );
}

/// Known members on a local type don't fire.
#[test]
fn known_local_member_no_error() {
    let src = "type Foo { x: int; static fn bar() {} }\nfn f(f: Foo) {\n    f.x;\n    var _ = Foo::bar;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .all(|m| !m.contains("has no member") && !m.contains("has no static member")),
        "got: {msgs:?}"
    );
}

/// Instance access on an enum is always wrong; the diagnostic points
/// the user at the `static_expr` form (`Enum::field`).
#[test]
fn instance_access_on_enum() {
    let src = "enum E { a, b }\nfn f(e: E) {\n    e.a;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m.contains("enum `E` has no instance members")
                && m.contains("access fields via `E::a`")),
        "got: {msgs:?}"
    );
}

/// Static access on an enum with a non-field name errors with a
/// targeted "no field `X`" message.
#[test]
fn static_access_on_enum_non_field() {
    let src = "enum E { a, b }\nfn f() {\n    var _ = E::nope;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter().any(|m| m == "enum `E` has no field `nope`"),
        "got: {msgs:?}"
    );
}

/// Valid enum field access via static_expr must not fire any error.
#[test]
fn enum_field_access_no_error() {
    let src = "enum E { a, b }\nfn f() {\n    var a = E::a;\n    var b = E::b;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(msgs.is_empty(), "got unexpected errors: {msgs:?}");
}

/// `any` is the escape hatch — member access on `any` / `any?` must
/// stay silent (no false positives on dynamic-typed code).
#[test]
fn any_receiver_silent() {
    let src = "fn f(x: any?) {\n    x.whatever;\n    x->whatever;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter().all(|m| !m.contains("has no member")),
        "expected no member errors on `any?`, got: {msgs:?}"
    );
}

/// Types whose body isn't loaded (stdlib not in the test harness)
/// stay silent — claiming "no member" without the full member set
/// would be a false positive.
#[test]
fn unloaded_type_silent() {
    // `String` is a primitive but no stdlib is loaded here, so
    // `type_members_for("String")` returns None and the gate skips emit.
    let src = "fn f(s: String) {\n    s.anything;\n}\n";
    let (pa, uri) = analyze(src);
    let msgs = error_messages(&pa, &uri);
    assert!(
        msgs.iter().all(|m| !m.contains("has no member")),
        "expected no member errors when type body isn't loaded, got: {msgs:?}"
    );
}
