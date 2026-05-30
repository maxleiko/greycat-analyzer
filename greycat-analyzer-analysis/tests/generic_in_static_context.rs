//! A *type-level* generic parameter (`type Foo<T>`) cannot be referenced
//! from inside a `static` method. A static carries no instance, so `T` is
//! unbound; GreyCat has no bounded generics, so the runtime can neither
//! construct (`T {}`) nor dispatch on it, and rejects any use. Flagged in
//! signature position (`: T`, `x: T`) and in method bodies (`T {}`).
//!
//! Method-level generics (`static fn f<U>()`) are the method's own and
//! stay valid; instance methods can use the type's `T` freely.

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

fn error_codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.to_string())
        .collect()
}

fn count(pa: &ProjectAnalysis, uri: &Uri, code: &str) -> usize {
    error_codes(pa, uri).iter().filter(|c| *c == code).count()
}

#[test]
fn return_type_generic_in_static_errors() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo<T> {\n    static fn make(): T? { return null; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        error_codes(&pa, &uri)
            .iter()
            .any(|c| c == "generic-in-static-context"),
        "expected `generic-in-static-context` on `static fn make(): T?`; got: {:?}",
        error_codes(&pa, &uri),
    );
}

#[test]
fn param_type_generic_in_static_errors() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo<T> {\n    static fn take(x: T): int { return 1; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        error_codes(&pa, &uri)
            .iter()
            .any(|c| c == "generic-in-static-context"),
        "expected `generic-in-static-context` on a static param typed `T`; got: {:?}",
        error_codes(&pa, &uri),
    );
}

#[test]
fn body_construction_and_return_type_each_flagged_once() {
    // `static fn make(): T { return T {}; }` — two distinct sites
    // (the `: T` annotation and the `T {}` construction), one error
    // each (single-emission despite `lower_type_ref` being a multi-call
    // helper).
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo<T> {\n    static fn make(): T { return T {}; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        count(&pa, &uri, "generic-in-static-context"),
        2,
        "expected exactly two `generic-in-static-context` (`: T` + `T {{}}`); got: {:?}",
        error_codes(&pa, &uri),
    );
}

#[test]
fn instance_method_generic_is_clean() {
    // Non-static method: `T` is bound by the instance, fully valid.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo<T> {\n    val: T?;\n    fn get(): T? { return this.val; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        !error_codes(&pa, &uri)
            .iter()
            .any(|c| c == "generic-in-static-context"),
        "instance method using `T` must be clean; got: {:?}",
        error_codes(&pa, &uri),
    );
}

#[test]
fn method_level_generic_in_static_is_clean() {
    // `static fn id<U>(x: U): U` — `U` is the method's own generic, not
    // the type's. Valid; must not be flagged.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/main.gcl",
        "type Foo<T> {\n    static fn id<U>(x: U): U { return x; }\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert!(
        !error_codes(&pa, &uri)
            .iter()
            .any(|c| c == "generic-in-static-context"),
        "method-level generic `U` must be clean; got: {:?}",
        error_codes(&pa, &uri),
    );
}
