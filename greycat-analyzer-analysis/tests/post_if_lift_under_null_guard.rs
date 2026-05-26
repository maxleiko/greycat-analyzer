//! Regression test for the P19.16 post-if assignment-narrow lift when
//! the binding was already narrowed to the literal `Null` shape by an
//! enclosing guard.
//!
//! Pattern:
//!
//!     var u: T? = ...;
//!     if (u == null) {              // outer guard: u: Null
//!         if (u == null) {          // inner guard
//!             u = nonNullExpr;      // then-branch assigns a real T
//!         }
//!         use(u);                   // u should be T (non-null)
//!     }
//!
//! Before the fix, the post-inner-if lift wrote `strip_nullable(pre)`
//! as the merged narrow. With `pre = Null`, `strip_nullable(Null)` is
//! a dead `Null (nullable=false)` kind — it passes the per-side
//! non-null check, but written as the post-if narrow it makes
//! downstream reads see `null`. The use site then reports
//! "Null not assignable to T" even though the analyzer's own merge
//! logic just proved both paths produce a non-null T.
//!
//! The fix detects the degenerate `pre = Null` case and prefers a
//! side's concrete narrow, falling back to the binding's declared
//! type (stripped of nullability) when both sides are dead too.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("not assignable")
                || d.message.contains("argument-type-mismatch")
                || d.message.contains("type-mismatch")
        })
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn assignment_lifts_null_to_non_null_under_outer_null_guard() {
    let src = "\
type User { name: String; }
fn make(): node<User> {
    var u: node<User>?;
    if (u == null) {
        if (u == null) {
            u = node<User> { User { name: \"x\" } };
        }
        return u;
    }
    return u;
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected u to be lifted to non-null after the inner `if (u == null) {{ u = nonNull }}`, got: {diags:?}",
    );
}

#[test]
fn nested_null_guards_with_nullable_intermediate_assignment() {
    // The original report's shape: an inner `if (other != null) { u =
    // possiblyNullExpr; }` leaves u nullable, then `if (u == null) { u
    // = nonNull }` should lift u to non-null. The outermost guard
    // ensures `pre = Null` is in play.
    let src = "\
type User { name: String; }
var by_email: nodeIndex<String, node<User>>;
fn getOrCreate(email: String?): node<User> {
    var u: node<User>?;
    if (u == null) {
        if (email != null) {
            u = by_email.get(email);
        }
        if (u == null) {
            u = node<User> { User { name: \"x\" } };
        }
        if (email != null) {
            by_email.set(email, u);
        }
    }
    return u;
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected u to be non-null after the second inner if, got: {diags:?}",
    );
}
