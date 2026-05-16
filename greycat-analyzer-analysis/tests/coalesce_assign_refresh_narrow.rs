//! Regression: `?=` (null-coalescing assignment) must refresh the
//! local's narrow when the prior narrow had pinned it to a bare
//! `null` shape (e.g. via a preceding `if (x == null)` guard).
//!
//! The kopr smoking gun is the classic "get-or-default" pattern:
//!
//! ```text
//! var gaussian = featureStats.get(i);  // Gaussian?
//! if (gaussian == null) {
//!     gaussian ?= Gaussian {};         // narrow should refresh
//!     featureStats.set(i, gaussian);   // expects non-null Gaussian
//! }
//! ```
//!
//! Before the fix, the inside-if narrow stayed pinned at the `null`
//! shape (the if-condition narrow), so the `set(i, gaussian)` call
//! flagged "value of type `null` is not assignable to parameter
//! `value: Gaussian`". The operator's semantics are
//! `x = x ?? rhs` — when `x` is currently `null`, the post-state is
//! exactly `rhs`. Stripping `nullable` off the `null` TypeId left
//! it still typed `null`; the fix replaces the narrow with the RHS
//! wholesale in that case.

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
fn coalesce_assign_refreshes_narrow_after_null_guard() {
    let mut mgr = SourceManager::new();
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Gaussian {}\n\
         fn take(g: Gaussian) {}\n\
         fn caller(g: Gaussian?) {\n\
             var local: Gaussian? = g;\n\
             if (local == null) {\n\
                 local ?= Gaussian {};\n\
                 take(local);\n\
             }\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "after `local ?= Gaussian {{}}` inside `if (local == null)`, `local` must be non-null; got: {:#?}",
        diags
    );
}

#[test]
fn coalesce_assign_to_nullable_does_not_force_non_null() {
    // Sanity: when the RHS is itself nullable, `?=` cannot guarantee
    // post-state non-null. The narrow should remain nullable. (Just
    // makes sure my refresh logic doesn't over-fire and swallow
    // legitimate "still nullable" cases.)
    let mut mgr = SourceManager::new();
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Gaussian {}\n\
         fn take(g: Gaussian) {}\n\
         fn maybe(): Gaussian? { return null; }\n\
         fn caller(g: Gaussian?) {\n\
             var local: Gaussian? = g;\n\
             if (local == null) {\n\
                 local ?= maybe();\n\
                 take(local);\n\
             }\n\
         }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "after `local ?= maybe()` the post-state must still be nullable (analyzer must refuse `take(local)`); silently accepted instead",
    );
}
