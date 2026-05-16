//! Regression tests for `fix(analyzer): apply condition truthy-narrows
//! to while / for body (B1, B2)`.
//!
//! Before the fix, `Stmt::While` / `Stmt::For` visited their body
//! without applying the loop condition's narrows. So
//! `while (x != null) { takesNonNull(x) }` and
//! `for (var p = init; p != null; ...) { takesNonNull(p) }` both
//! reported `int? not assignable to int` inside the body, even though
//! the loop condition guarantees non-null at body entry.
//!
//! The fix pushes a narrow frame, lifts the condition's then-side
//! `CondNarrows` into it, then inlines the body stmts so the loop's
//! narrow is the innermost frame at body entry (mirrors `Stmt::If`).
//!
//! `do-while` is intentionally NOT covered — its body runs before the
//! condition is ever checked (iter 1), so applying the cond narrow
//! would be unsound. See `do_while_does_not_narrow_body`.

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
fn while_null_guard_narrows_body() {
    // `while (x != null) { takesNonNull(x); }` — body sees x as int.
    let src = "\
fn takesNonNull(x: int) {}
fn main() {
    var x: int? = 3;
    while (x != null) {
        takesNonNull(x);
        x = null;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn while_conjunction_narrows_body() {
    // `while (x != null && x > 0)` — `x` is non-null in body via the
    // `&&` arm's union of subtree narrows.
    let src = "\
fn takesNonNull(x: int) {}
fn main() {
    var x: int? = 3;
    while (x != null && x > 0) {
        takesNonNull(x);
        x = null;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn for_c_style_condition_narrows_body_and_increment() {
    // Kopr B1 shape: increment reads `p` which must be narrowed too.
    let src = "\
fn nextOf(x: int): int? {
    if (x > 0) { return x - 1; }
    return null;
}
fn takesNonNull(x: int) {}
fn main() {
    var target: int? = 3;
    for (var p = target; p != null; p = nextOf(p)) {
        takesNonNull(p);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn for_c_style_narrow_does_not_leak_after_loop() {
    // The narrow holds only inside the loop's frame; outside, `p` is
    // back to its declared nullable type.
    let src = "\
fn nextOf(x: int): int? { return x; }
fn takesNonNull(x: int) {}
fn main() {
    var target: int? = 3;
    for (var p = target; p != null; p = nextOf(p)) {
        takesNonNull(p);
    }
    takesNonNull(target);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic for `takesNonNull(target)` after the loop, got none",
    );
}

#[test]
fn while_is_guard_narrows_body() {
    // `while (v is Child) { takesChild(v); }` — `then_typed` narrow
    // path applied to body.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
fn takesChild(c: Child) {}
fn main() {
    var v: Base = Child { name: \"x\", extra: 1 };
    while (v is Child) {
        takesChild(v);
        return;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn do_while_does_not_narrow_body() {
    // `do { ... } while (p != null);` — iter 1 runs before the cond
    // is checked, so applying the truthy narrow to the body would be
    // unsound. The analyzer correctly leaves `p` nullable here.
    let src = "\
fn takesNonNull(x: int) {}
fn main() {
    var p: int? = 3;
    do {
        takesNonNull(p);
    } while (p != null);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic for do-while body (cond not yet established on iter 1), got none",
    );
}

#[test]
fn for_body_reassigns_narrowed_binding() {
    // Body-side reassignment to nullable invalidates the cond narrow
    // for the rest of the iteration via the innermost-frame-wins
    // lookup rule. `record_assign_narrow` writes the RHS's type into
    // the innermost (body's own) frame, shadowing the loop frame.
    let src = "\
fn nextOf(x: int): int? { return x; }
fn takesNonNull(x: int) {}
fn main() {
    var target: int? = 3;
    for (var p = target; p != null; p = nextOf(p)) {
        p = null;
        takesNonNull(p);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic after the body reassigns `p` to null, got none",
    );
}

#[test]
fn for_no_condition_does_not_panic() {
    // `for (init; ; incr) body` — no condition means no narrow to
    // derive. The `CondNarrows::default()` path keeps the analyzer
    // from panicking on the missing condition.
    let src = "\
fn nextOf(x: int): int? { return x; }
fn main() {
    var target: int? = 3;
    for (var p = target; ; p = nextOf(p)) {
        if (p == null) { return; }
    }
}
";
    let (uri, pa) = analyze(src);
    // Don't care about diagnostic shape — just that analysis completes.
    let _ = assignability_diagnostics(&pa, &uri);
}
