//! Regression test for do-while body assignments being invisible to
//! the loop condition.
//!
//! Before the fix, `Stmt::DoWhile` called `visit_block` on the body,
//! which pushes + pops its own narrow frame. The frame's contents
//! (including any `id = nonNull` assignment narrow inside the body)
//! disappeared before the condition was visited, so the cond's reads
//! of `id` still saw whatever the *outer* scope's narrow said —
//! typically `null`, when the loop sits inside an `if (id == null)`
//! guard.
//!
//! The fix inlines the body's stmts inside a dedicated narrow frame
//! that stays alive for both the body and the condition, then
//! captures the cond's narrows and pops.

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
        .filter(|d| d.message.contains("not assignable"))
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn do_while_body_assignment_visible_to_condition() {
    // The original report: inside `if (id == null)`, the body
    // reassigns id to a non-null String, then the cond reads id.
    // Cond must see the body's assignment.
    let src = "\
var sessions: nodeIndex<String, node<User>>;
type User {}
fn generateId(): String { return \"abc\"; }
fn createSession(id: String?) {
    if (id == null) {
        do {
            id = generateId();
        } while (sessions.get(id) != null);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected the do-while body's `id = generateId()` to narrow id non-null for the cond, got: {diags:?}",
    );
}

#[test]
fn do_while_body_assignment_typed_argument() {
    // Same shape but with an explicit non-nullable typed parameter
    // call inside the cond — ensures the narrow propagates through a
    // direct argument-type check, not just a member lookup.
    let src = "\
fn needsString(s: String): bool { return true; }
fn run(id: String?) {
    if (id == null) {
        do {
            id = \"hi\";
        } while (needsString(id));
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected id to be narrowed to String for the cond after the body assignment, got: {diags:?}",
    );
}

#[test]
fn do_while_body_local_var_does_not_leak() {
    // Local vars declared inside the body must stay scoped to the
    // body — the body's narrow frame is dedicated to the loop, but
    // local *declarations* don't survive the pop. (The test passes
    // when the resolver does its job; here we just verify the new
    // inlined-body shape didn't accidentally widen scope.)
    let src = "\
fn run(): int {
    do {
        var inside: int = 1;
    } while (false);
    return inside;
}
";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).expect("module");
    let unresolved = m
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("inside"))
        .count();
    assert!(
        unresolved > 0,
        "expected `inside` to be unresolved post-loop, got no related diagnostic",
    );
}
