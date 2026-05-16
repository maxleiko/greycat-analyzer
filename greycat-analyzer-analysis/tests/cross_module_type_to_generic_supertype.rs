//! Regression: a non-generic concrete `Sub extends Base<Concrete>`
//! must be assignable to a `Base<any?>`-typed parameter / collection
//! slot. The GreyCat runtime (`greycat run` 8.0.322-dev) accepts this
//! shape — the analyzer used to reject it because the inheritance-
//! aware `Type → Generic` arm in `is_assignable_to_with_index` was
//! missing: only `Type → Type` and `Generic → Generic` with matching
//! head-decl were honored.
//!
//! Real-world incidence (kopr): many places build
//! `Array<AbstractView<any?>> {}` and then `add(SubViewType { ... })`,
//! where `SubViewType extends AbstractView<Concrete>`. Each such
//! `add` site fired a "not assignable" diagnostic until the chain
//! walk was added.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
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
fn nongeneric_subtype_assignable_to_generic_supertype_with_wildcard_args() {
    // `Sub extends Base<int>`; consume site expects `Base<any?>`.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base<T> {\n    val: T;\n}\n\
         type Sub extends Base<int> {\n    extra: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<any?>) {}\n\
         fn caller() {\n    take(Sub { val: 1, extra: 2 });\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Sub should be assignable to Base<any?>, got: {:#?}",
        diags
    );
}

#[test]
fn nongeneric_subtype_assignable_to_generic_supertype_same_concrete_arg() {
    // `Sub extends Base<int>`; consume site expects `Base<int>`
    // (the actual chain instantiation). Without chain walking we
    // wouldn't see the link from `Sub` (a plain `Type`) to the
    // `Generic { decl: Base, args: [int] }` target shape.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base<T> {\n    val: T;\n}\n\
         type Sub extends Base<int> {\n    extra: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<int>) {}\n\
         fn caller() {\n    take(Sub { val: 1, extra: 2 });\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Sub should be assignable to Base<int>, got: {:#?}",
        diags
    );
}

#[test]
fn unrelated_concrete_arg_still_rejected() {
    // `Sub extends Base<int>`; consume site expects `Base<String>`.
    // The chain walk must NOT silently accept this — substitution
    // along the chain produces `Base<int>`, which is not assignable
    // to `Base<String>` (the all-Any wildcard rule does not apply).
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base<T> {\n    val: T;\n}\n\
         type Sub extends Base<int> {\n    extra: int;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<String>) {}\n\
         fn caller() {\n    take(Sub { val: 1, extra: 2 });\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Sub : Base<int> must NOT be assignable to Base<String>; analyzer let it through silently",
    );
}
