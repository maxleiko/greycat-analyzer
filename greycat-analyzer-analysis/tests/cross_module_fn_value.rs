//! P38.1 — cross-module bare fn ident types as `function`, not `type`.
//!
//! Before the fix, a bare reference to a fn declared in another
//! module (resolved by the resolver to `Definition::ProjectDecl`)
//! lowered to the `type` value because the ident-typing arm only
//! consulted `fn_signatures` (natives only). Non-native fns landed in
//! `values` and fell through to `has_name → type_ty()`. The runtime
//! (8.0.291-dev) accepts the same shape — `greycat build` exit 0 on
//! `Scheduler::add(fetch_stuff, …)` with `fn fetch_stuff() {}` in a
//! sibling module.

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
fn bare_cross_module_fn_ident_types_as_function() {
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/helper.gcl", "fn fetch_stuff() {}\n");
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn takes_fn(f: function) {}
fn caller() {
    takes_fn(fetch_stuff);
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
fn private_cross_module_fn_via_fqn_types_as_function() {
    // FQN form is the runtime-allowed escape hatch for private — the
    // bare form is unresolved across modules (covered by 38.4); the
    // FQN form must still type as `function`. Mirrors the existing
    // [`private_fn_module_qualifier`] regression, kept here so a
    // cross-module-fn-typing regression flags this file too.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/helper.gcl", "private fn my_fn() {}\n");
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn takes_fn(f: function) {}
fn caller() {
    takes_fn(helper::my_fn);
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
