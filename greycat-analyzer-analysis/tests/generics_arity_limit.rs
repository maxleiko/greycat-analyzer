//! The GreyCat runtime caps generic-parameter arity differently per
//! kind: a *type* accepts two (`Map<K, V>` is the widest), a *function*
//! accepts exactly one — `fn f<A, B>(...)` is a runtime *syntax error*.
//! The grammar accepts any arity, so the analyzer enforces both ceilings
//! and emits a Severity::Error `too-many-generics` diagnostic.

use greycat_analyzer_analysis::analyzer::Severity;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn errors_in(pa: &ProjectAnalysis, uri: &Uri, needle: &str) -> usize {
    pa.module(uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error && d.message.contains(needle))
        .count()
}

#[test]
fn type_decl_with_three_generics_is_flagged() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "type Triple<A, B, C> {\n    a: A;\n    b: B;\n    c: C;\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "supports at most 2"),
        1,
        "expected one >2-generics error: {:?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn fn_decl_with_two_generics_is_flagged() {
    // A function accepts exactly one generic — two is already over the
    // ceiling (and a runtime syntax error).
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "fn swap<A, B>(a: A, b: B): A { return a; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "supports at most 1"),
        1,
        "expected one >1-fn-generics error: {:?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn fn_decl_with_one_generic_is_ok() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "fn id<T>(x: T): T { return x; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "generic parameters"),
        0,
        "a single fn generic must not trip the arity check: {:?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn type_with_two_generics_is_the_supported_ceiling() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "type Pair<A, B> {\n    a: A;\n    b: B;\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "generic parameters"),
        0,
        "a 2-generic type must not trip the arity check: {:?}",
        pa.module(&uri).unwrap().analysis.diagnostics
    );
}

#[test]
fn nongeneric_decls_are_not_flagged() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "type Foo { a: int; }\nfn bar(): int { return 0; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "supports at most 2"),
        0,
        "no-generic decls must not be flagged"
    );
}
