//! Regression: a generic fn that constructs and returns a container
//! parameterized by its own generic (`fn wrap<T>(x: T): Box<T>`) — or a
//! method using the enclosing type's generic (`type Box<T> { fn dup():
//! Box<T> }`) — must NOT produce a spurious return-type-mismatch.
//!
//! The body-validation return type must be lowered with the fn's (and,
//! for a method, the enclosing type's) generics in scope, so it stays
//! `Box<T>` (`GenericParam`) instead of collapsing `T` to `any?` /
//! `Unresolved` and mismatching the body's `Box<T>`-typed `return`. Both
//! shapes confirmed valid GreyCat via `greycat run` 8.0.372-dev.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn type_mismatches(src: &str) -> Vec<String> {
    let uri = Uri::from_str("file:///m.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add_simple(uri.clone(), src, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    pa.module(&uri)
        .expect("module")
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "type-mismatch")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn generic_fn_constructing_container_return_is_clean() {
    let d = type_mismatches(
        "type Box<T> { item: T; }\n\
         fn wrap<T>(x: T): Box<T> { return Box<T> { item: x }; }\n",
    );
    assert!(d.is_empty(), "spurious return-type-mismatch: {d:?}");
}

#[test]
fn generic_fn_constructing_container_via_var_is_clean() {
    let d = type_mismatches(
        "type Box<T> { item: T; }\n\
         fn wrap<T>(x: T): Box<T> { var b = Box<T> { item: x }; return b; }\n",
    );
    assert!(d.is_empty(), "spurious return-type-mismatch via var: {d:?}");
}

#[test]
fn method_using_type_generic_in_container_return_is_clean() {
    let d = type_mismatches(
        "type Box<T> {\n\
        \x20   item: T;\n\
        \x20   fn dup(): Box<T> { return Box<T> { item: this.item }; }\n\
         }\n",
    );
    assert!(
        d.is_empty(),
        "spurious return-type-mismatch on method: {d:?}"
    );
}

#[test]
fn bare_generic_return_is_clean() {
    let d = type_mismatches(
        "type Box<T> { item: T; }\n\
         fn first<T>(b: Box<T>): T { return b.item; }\n",
    );
    assert!(d.is_empty(), "bare-T return should be clean: {d:?}");
}

#[test]
fn real_return_type_mismatch_still_fires() {
    // The fix must not silence genuine mismatches.
    let d = type_mismatches("fn bad(): int { return \"hi\"; }\n");
    assert_eq!(
        d.len(),
        1,
        "genuine return-type-mismatch must still fire: {d:?}"
    );
}
