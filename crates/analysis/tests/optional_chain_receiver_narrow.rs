//! Regression tests for optional-chain receiver narrowing.
//!
//! Before the fix, `derive_cond_narrows` recorded the *full* chain
//! path in `then_member_non_null` for `chain != null` shapes, but
//! never propagated the implied non-null-ness to inner receivers.
//! So `if (a.b?->c != null) { a.b->c }` would still flag `a.b` as
//! possibly null inside the body, because the inner Member access
//! evaluated `a.b` with its declared nullable type.
//!
//! The fix walks the Member/Arrow chain at `derive_cond_narrows` time
//! and lifts each `?->`/`?.` receiver to non-null on the matching
//! branch — sound because `?->` short-circuits to null iff the
//! receiver is null, so chain-non-null ⇒ every `?->` receiver
//! non-null.

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

fn diagnostic_messages_matching(pa: &ProjectAnalysis, uri: &Uri, needle: &str) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains(needle))
        .map(|d| d.message.clone())
        .collect()
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    diagnostic_messages_matching(pa, uri, "not assignable")
}

fn possibly_null_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    diagnostic_messages_matching(pa, uri, "is possibly `null`")
}

#[test]
fn arrow_chain_eq_string_narrows_receiver() {
    // Shape: `if (o->next?->name == "x") { use(o->next); }`. The outer
    // `?->` step's receiver `o->next` should be non-null in the body
    // since the chain's value is checked equal to a non-null literal.
    let src = "\
type Inner { name: String; }
type Outer { next: node<Inner>?; }
fn use_(x: node<Inner>) { let _ = x; }
fn main() {
    var inner = node<Inner> { Inner { name: \"x\" } };
    var o = node<Outer> { Outer { next: inner } };
    if (o->next?->name == \"x\") {
        use_(o->next);
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
fn arrow_chain_neq_null_narrows_receiver() {
    // User-supplied reproducer: `if (c.sim?->points_by_geo != null) {
    // c.sim->points_by_geo }`. The inner `c.sim` access must see
    // `c.sim` as non-null in the body.
    let src = "\
type Ctx { sim: node<Simulation>?; }
type Simulation { points_by_geo: nodeGeo<Tuple<float, float>>; }
fn test(c: Ctx) {
    if (c.sim?->points_by_geo != null) {
        var _ = c.sim->points_by_geo;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = possibly_null_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no `possibly null` diagnostics, got: {diags:?}"
    );
}

#[test]
fn dot_chain_eq_string_narrows_receiver() {
    // Same as #1 but with `.` instead of `->` (`?.` instead of `?->`).
    let src = "\
type Inner { name: String; }
type Outer { next: Inner?; }
fn use_(x: Inner) { let _ = x; }
fn main() {
    var o = Outer { next: Inner { name: \"x\" } };
    if (o.next?.name == \"x\") {
        use_(o.next);
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
fn mixed_arrow_dot_chain_narrows_receiver() {
    // `if (o.foo?->bar?->baz != null) { use(o.foo); }` — the outer
    // `?->` (over `o.foo->bar`) and inner `?->` (over `o.foo`) both
    // contribute. Specifically `o.foo` (a Member access) gets a path
    // narrow.
    let src = "\
type Bar { baz: int; }
type Foo { bar: node<Bar>?; }
type O { foo: node<Foo>?; }
fn use_(x: node<Foo>) { let _ = x; }
fn main() {
    var o = O { foo: null };
    if (o.foo?->bar?->baz != null) {
        use_(o.foo);
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
fn chain_narrow_does_not_leak_after_if() {
    // Outside the if's narrow frame, the receiver should still be
    // nullable. Pure-types fixture (no `node<>`) so the
    // `possibly-null` lint actually runs.
    let src = "\
type Inner { name: String; }
type Outer { next: Inner?; }
fn use_(x: Inner) { let _ = x; }
fn main() {
    var o = Outer { next: Inner { name: \"x\" } };
    if (o.next?.name == \"x\") {
    }
    use_(o.next);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic for the access after the if, got none",
    );
}

#[test]
fn chain_eq_null_narrows_receiver_in_else() {
    // `if (c.sim?->p == null) { } else { c.sim->p }` — else-branch
    // sees `c.sim` as non-null.
    let src = "\
type Ctx { sim: node<Simulation>?; }
type Simulation { points_by_geo: nodeGeo<Tuple<float, float>>; }
fn test(c: Ctx) {
    if (c.sim?->points_by_geo == null) {
    } else {
        var _ = c.sim->points_by_geo;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = possibly_null_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no `possibly null` diagnostics in the else branch, got: {diags:?}"
    );
}

#[test]
fn chain_eq_nullable_var_does_not_narrow() {
    // `chain == someVar` where `someVar` might be null — we cannot
    // conclude the chain is non-null. The receiver stays nullable.
    // Pure-types fixture so the assignability check actually runs.
    let src = "\
type Inner { name: String; }
type Outer { next: Inner?; }
fn use_(x: Inner) { let _ = x; }
fn main() {
    var maybe: String? = null;
    var o = Outer { next: null };
    if (o.next?.name == maybe) {
        use_(o.next);
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic since the comparison target is nullable, got none",
    );
}

#[test]
fn chain_eq_enum_variant_narrows_receiver() {
    // Shape: `if (a.b?->c?->kind == E::v) { use(a.b); }`. RHS is
    // `Expr::Static` (enum variant), syntactically non-null — every
    // `?->` receiver in the chain should be lifted to non-null.
    let src = "\
enum Kind { alpha, beta }
type Leaf { kind: Kind; }
type Mid { leaf: node<Leaf>?; }
type Root { mid: node<Mid>?; }
fn use_(x: node<Mid>) { let _ = x; }
fn main() {
    var l = node<Leaf> { Leaf { kind: Kind::alpha } };
    var m = node<Mid> { Mid { leaf: l } };
    var r = Root { mid: m };
    if (r.mid?->leaf?->kind == Kind::alpha) {
        use_(r.mid);
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
fn chain_narrow_composes_with_while_body_narrow() {
    // `while (c.sim?->p != null) { use(c.sim->p); }`: the while-body
    // narrow lift and the chain-receiver lift compose. The while-body
    // pass picks up the chain receiver in `then_member_non_null` and
    // applies it to the body's frame.
    let src = "\
type Ctx { sim: node<Simulation>?; }
type Simulation { points_by_geo: nodeGeo<Tuple<float, float>>; }
fn test(c: Ctx) {
    while (c.sim?->points_by_geo != null) {
        var _ = c.sim->points_by_geo;
        return;
    }
}
";
    let (uri, pa) = analyze(src);
    let diags = possibly_null_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no `possibly null` diagnostics, got: {diags:?}"
    );
}
