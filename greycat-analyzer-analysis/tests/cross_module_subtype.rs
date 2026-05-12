//! P38.2 — cross-module `extends` chain is honored by assignability.
//!
//! Before the fix, the analyzer's in-module `lower_type_ref` minted
//! `Named(name)` for a foreign non-generic type while
//! `lower_type_ref_project` (used for foreign fn signatures) minted
//! `Type(handle)`. The asymmetric pair defeated
//! `is_assignable_to_with_index`'s `(Type, Type)` extends-walk arm.
//! The runtime (8.0.291-dev) accepts the same shape — `greycat build`
//! exit 0 on `takes_base(Sub {})` where `Sub extends Base` and both
//! live in a sibling module.

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
fn cross_module_subtype_is_assignable_to_supertype() {
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base {}
type Sub extends Base {}
",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn takes_base(b: Base): String { return \"got\"; }
fn caller() {
    takes_base(Sub {});
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_chained_subtype_is_assignable() {
    // `GrandSub extends Sub extends Base` — recursive walk must
    // traverse multiple hops across module boundaries.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base {}
type Sub extends Base {}
type GrandSub extends Sub {}
",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn takes_base(b: Base): String { return \"got\"; }
fn caller() {
    takes_base(GrandSub {});
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors, got: {:#?}",
        diags
    );
}

#[test]
fn cross_module_unrelated_type_still_reports() {
    // Negative regression: a type that is NOT in the supertype chain
    // must still surface the "not assignable" error.
    let mut mgr = SourceManager::new();
    add(
        &mut mgr,
        "/proj/src/types.gcl",
        "abstract type Base {}
type Other {}
",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn takes_base(b: Base): String { return \"got\"; }
fn caller() {
    takes_base(Other {});
}
",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert_eq!(
        diags.len(),
        1,
        "expected one assignability error, got: {:#?}",
        diags
    );
    assert!(diags[0].contains("Other"));
    assert!(diags[0].contains("Base"));
}
