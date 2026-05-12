//! The GreyCat runtime supports at most 2 generic parameters
//! (`Map<K, V>` is the widest). The grammar accepts any arity, so the
//! analyzer enforces the runtime ceiling and emits a Severity::Error
//! diagnostic on `type Foo<A, B, C> {}` / `fn f<A, B, C>(...) {}`.

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
fn fn_decl_with_three_generics_is_flagged() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "fn merge<A, B, C>(a: A, b: B, c: C): A { return a; }\n",
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
fn two_generics_is_the_supported_ceiling() {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(
        uri.clone(),
        "type Pair<A, B> {\n    a: A;\n    b: B;\n}\n\
         fn swap<A, B>(a: A, b: B): A { return a; }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    assert_eq!(
        errors_in(&pa, &uri, "supports at most 2"),
        0,
        "2-generic decls must not trip the arity check: {:?}",
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
