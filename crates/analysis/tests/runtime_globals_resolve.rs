//! Regression: the runtime-exposed float globals `Infinity` / `NaN`
//! must resolve (no `unresolved-name`) and carry their `float` type
//! into downstream inference.
//!
//! `ProjectIndex` seeds these into `runtime_globals`, and the
//! analyzer's `Definition::Project` arm types them as `float`. But the
//! resolver only reaches that arm when `has_name` recognises the
//! symbol. `has_name` had stopped consulting `runtime_globals`, so the
//! names fell through to `unresolved`, got typed `any?`, and any
//! generic inference fed by them then reported a spurious
//! `cannot infer T` conflict (`any?` vs `float`).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn messages(pa: &ProjectAnalysis, uri: &Uri, needle: &str) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains(needle))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn infinity_and_nan_resolve_as_float() {
    let mut mgr = SourceManager::new();
    // `pick<T>(a, b)` reproduces the downstream generic-inference path
    // (Case 2 of the demo): if `Infinity` types as `any?`, unifying it
    // with `float` reports a conflict; if it types as `float`, T=float.
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn pick<T>(a: T, b: T): T { return a; }\n\
         fn use_globals(v: float): float {\n\
             var m = Infinity;\n\
             var n = NaN;\n\
             var ni = -Infinity;\n\
             m = pick(m, v);\n\
             return m + n + ni;\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);

    let unresolved = messages(&pa, &uri, "unresolved");
    assert!(
        unresolved.is_empty(),
        "`Infinity` / `NaN` must resolve as runtime globals; got: {:#?}",
        unresolved
    );

    let conflicts = messages(&pa, &uri, "cannot infer");
    assert!(
        conflicts.is_empty(),
        "`Infinity` must type as `float`, so unifying it with a `float` arg infers T=float without conflict; got: {:#?}",
        conflicts
    );
}
