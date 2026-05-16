//! Regression tests for "definitely null" branch narrows in `Stmt::If`.
//!
//! Before the fix, `x != null` populated only `then_non_null`; the
//! else side knew x must be null but stored nothing. So
//! `if (x != null) { return; } use(x)` post-if saw `x: int?` instead
//! of `x: Null`, and `if (x != null) { } else { use(x as int); }`
//! treated x as int? in the else branch despite the cond.
//!
//! The fix adds `then_null` / `else_null` / `then_member_null` /
//! `else_member_null` marker fields populated symmetrically with
//! the non_null lists. The `Stmt::If` then-entry / else-entry /
//! early-exit-lift sites materialize them via `arena.null()`.

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
fn if_neq_null_else_branch_sees_null() {
    // `if (x != null) {} else { var y: int = x; }` — else-branch
    // narrows x to Null. Assigning to a non-nullable `int` must
    // error.
    let src = "\
fn main() {
    var x: int? = 3;
    if (x != null) {
    } else {
        var y: int = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic in the else branch (Null not assignable to int), got none",
    );
}

#[test]
fn if_eq_null_then_branch_sees_null() {
    // `if (x == null) { var y: int = x; }` — then-branch narrows
    // x to Null.
    let src = "\
fn main() {
    var x: int? = 3;
    if (x == null) {
        var y: int = x;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic in the then branch (Null not assignable to int), got none",
    );
}

#[test]
fn if_neq_null_terminating_then_post_if_sees_null() {
    // `if (x != null) { return; } var y: int = x;` — then-branch
    // terminates, so the early-exit lift drops `else_null` into
    // the post-if scope. `y: int = x` errors.
    let src = "\
fn main() {
    var x: int? = 3;
    if (x != null) {
        return;
    }
    var y: int = x;
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic post-if (Null not assignable to int), got none",
    );
}

#[test]
fn if_eq_null_terminating_then_post_if_non_null_unchanged() {
    // Regression: existing `else_non_null` lift still works. After
    // `if (x == null) { return; }`, x is non-null post-if. `var y:
    // int = x` must remain clean.
    let src = "\
fn main() {
    var x: int? = 3;
    if (x == null) {
        return;
    }
    var y: int = x;
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostic post-if (x is non-null via existing else_non_null lift), got: {diags:?}",
    );
}

#[test]
fn if_eq_null_member_path_then_sees_null() {
    // Member-path version: `if (this.x == null) { var y: int =
    // this.x; }` — then-branch narrows the member path to Null.
    let src = "\
type Holder { x: int?; }
type Container {
    h: Holder;
    fn doStuff() {
        if (this.h.x == null) {
            var y: int = this.h.x;
        }
    }
}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic on the member-path then-branch read, got none",
    );
}

#[test]
fn if_neq_null_terminating_then_post_if_member_sees_null() {
    // Member-path version of the early-exit lift:
    // `if (this.x != null) { return; } var y: int = this.x;` —
    // the lift drops `else_member_null` into post-if scope.
    let src = "\
type Holder { x: int?; }
type Container {
    h: Holder;
    fn doStuff() {
        if (this.h.x != null) {
            return;
        }
        var y: int = this.h.x;
    }
}
fn main() {}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic on the post-if member-path read, got none",
    );
}

#[test]
fn if_branch_null_invalidated_by_reassignment() {
    // After `if (x != null) { return; }` lifts `else_null`, the
    // post-if x is Null. Re-assigning x writes the innermost frame,
    // shadowing the Null narrow. `var y: int = x` after that must
    // be clean.
    let src = "\
fn main() {
    var x: int? = 3;
    if (x != null) {
        return;
    }
    x = 5;
    var y: int = x;
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostic after re-assignment overrides the Null narrow, got: {diags:?}",
    );
}
