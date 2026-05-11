//! Regression tests for `fix(analysis): is-narrowing on member-access
//! paths, !is swap`.
//!
//! Before the fix, `derive_cond_narrows`'s `Expr::Is` arm only
//! recognised an `Ident` operand, so a guard like
//! `if (holder.field is Sub) { takesSub(holder.field); }` left the
//! body still seeing the receiver's declared (super) type. The
//! analyzer surfaced "value of `Super` is not assignable to `Sub`".
//!
//! The other half of the fix lets `if (!(x is T)) { throw }; use(x);`
//! lift the narrow past the early-throw guard. That hinges on
//! `Expr::Unary { op: Not }` swapping then↔else *and* on the
//! `then_terminates` block applying `else_typed` /
//! `else_member_typed`.

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
fn is_narrow_on_member_field_narrows_receiver_type() {
    // `if (h.f is Child) { takesChild(h.f); }` — the body must see
    // `h.f` as `Child`, not `Base`.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
type Holder { f: Base; }
fn takesChild(c: Child) {}
fn main() {
    var h = Holder { f: Child { name: \"x\", extra: 42 } };
    if (h.f is Child) {
        takesChild(h.f);
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
fn is_narrow_on_arrow_path_narrows_inner_type() {
    // Same shape via `->` (the path key keeps the operator distinct,
    // so `h->f is Child` records "h->f -> Child" and `h->f` reads
    // the same key).
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
type Holder { f: Base; }
fn takesChild(c: Child) {}
fn main() {
    var h = node<Holder> { Holder { f: Child { name: \"x\", extra: 42 } } };
    if (h->f is Child) {
        takesChild(h->f);
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
fn not_is_throw_lifts_narrow_to_post_if_scope() {
    // `if (!(h.f is Child)) { throw "x" }; takesChild(h.f);` — the
    // post-if scope inherits the else side of the narrow, so `h.f`
    // is `Child` after the if.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
type Holder { f: Base; }
fn takesChild(c: Child) {}
fn main() {
    var h = Holder { f: Child { name: \"x\", extra: 42 } };
    if (!(h.f is Child)) {
        throw \"wrong type\";
    }
    takesChild(h.f);
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
fn is_narrow_on_ident_still_works() {
    // Regression guard for the existing `Expr::Ident` narrow path —
    // the member-path additions must not break the simple form.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
fn takesChild(c: Child) {}
fn main() {
    var v: Base = Child { name: \"x\", extra: 1 };
    if (v is Child) {
        takesChild(v);
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
fn is_narrow_member_path_does_not_leak_outside_then_branch() {
    // After the if-then closes, the member-path narrow must go away.
    // Calling `takesChild(h.f)` *outside* the guard should still
    // fail (h.f is back to its declared `Base`).
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
type Holder { f: Base; }
fn takesChild(c: Child) {}
fn main() {
    var h = Holder { f: Child { name: \"x\", extra: 42 } };
    if (h.f is Child) {
        takesChild(h.f);
    }
    takesChild(h.f);
}
";
    let (uri, pa) = analyze(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic for the call outside the guard, got none"
    );
}
