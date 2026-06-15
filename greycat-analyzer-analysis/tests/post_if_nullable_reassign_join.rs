//! Regression test for Bug 2 — a reassignment inside a conditional
//! branch must not be dropped by the post-if narrow-frame join when the
//! join stays nullable.
//!
//! Pattern:
//!
//!     var u = byName.get(name);     // u: node<User>?
//!     if (u != null) { return u; }  // fall-through narrows u to `null`
//!     if (cond) {
//!         u = byEmail.get(email);   // reassign u to node<User>?
//!     }
//!     if (u != null) { return u; }  // <-- u must be node<User>? here
//!
//! Before the fix, the post-if join only wrote a narrow when the binding
//! was non-null on *every* path (the "lift" case). The reassignment left
//! `u` nullable, so the lift didn't fire and the stale `null` narrow from
//! the first guard survived. The third `if (u != null) { return u; }`
//! then saw `u` as `null` and reported "null not assignable to node<User>".

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
        .filter(|d| d.message.contains("not assignable") || d.message.contains("type-mismatch"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn reassign_in_branch_survives_post_if_join_under_null_fallthrough() {
    let src = "\
type User { name: String; }
var by_name: nodeIndex<String, node<User>>;
var by_email: nodeIndex<String, node<User>>;
fn upsert(name: String, email: String?): node<User> {
    var u = by_name.get(name);
    if (u != null) {
        return u;
    }
    if (email != null) {
        u = by_email.get(email);
    }
    if (u != null) {
        return u;
    }
    return node<User> { User { name: name } };
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "reassignment of `u` inside `if (email != null)` must reach the third guard, got: {diags:?}",
    );
}
