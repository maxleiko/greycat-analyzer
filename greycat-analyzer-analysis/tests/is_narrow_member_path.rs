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

/// Same as [`analyze`] but seeds a synthetic `std/core` so the offset
/// arm's `well_known.array_decl` slot is populated. Required for any
/// test that exercises `arr[N]` typing — without std loaded the offset
/// arm falls back to `any`, which silences the negative-shape
/// diagnostics we want to assert.
fn analyze_with_std(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(
        std_uri,
        "native type any {}\n\
         native type null {}\n\
         native type bool {}\n\
         native type int {}\n\
         native type float {}\n\
         native type String {}\n\
         native type Array<T> {}\n",
        "std",
        false,
    );
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

fn unknown_member_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "unknown-member")
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
fn is_narrow_on_offset_with_literal_index_narrows_element() {
    // `if (arr[0] is Child) { takesChild(arr[0]); }` — the body must
    // see `arr[0]` as `Child`, not the declared element type `Base`.
    // This is the ifc_test.gcl shape: a guard on `e.attrs[0]` followed
    // by `e.attrs[0].value` access on the narrowed subtype.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
fn takesChild(c: Child) {}
fn main() {
    var arr: Array<Base> = Array<Base> { Child { name: \"x\", extra: 1 } };
    if (arr[0] is Child) {
        takesChild(arr[0]);
    }
}
";
    let (uri, pa) = analyze_with_std(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability diagnostics, got: {diags:?}"
    );
}

#[test]
fn is_narrow_on_offset_lets_member_resolve() {
    // After `arr[0] is Child`, `arr[0].extra` must resolve against
    // `Child`'s members (not `Base`'s). Without the offset-path narrow,
    // the analyzer would emit `unknown-member: type 'Base' has no
    // member 'extra'` — that's the ifc_test.gcl regression shape.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
fn main() {
    var arr: Array<Base> = Array<Base> { Child { name: \"x\", extra: 1 } };
    if (arr[0] is Child) {
        var _x = arr[0].extra;
    }
}
";
    let (uri, pa) = analyze_with_std(src);
    let diags = unknown_member_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected member access to resolve under offset narrow, got: {diags:?}"
    );
}

#[test]
fn is_narrow_on_offset_distinct_indices_dont_share() {
    // `arr[0] is ChildA` should NOT narrow `arr[1]` — the path keys
    // `arr[0]` and `arr[1]` are distinct, so a guard on one element
    // can't lift the type on a sibling element. Uses two concrete
    // children so P42's abstract-sealed narrow doesn't collapse the
    // unguarded `arr[1]` read to the single concrete subtype.
    let src = "\
abstract type Base { name: String; }
type ChildA extends Base { extra: int; }
type ChildB extends Base { tag: String; }
fn takesChildA(c: ChildA) {}
fn main() {
    var arr: Array<Base> = Array<Base> {
        ChildA { name: \"x\", extra: 1 },
        ChildB { name: \"y\", tag: \"t\" }
    };
    if (arr[0] is ChildA) {
        takesChildA(arr[1]);
    }
}
";
    let (uri, pa) = analyze_with_std(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected an assignability diagnostic on arr[1] (sibling, not narrowed), got: {diags:?}"
    );
}

#[test]
fn is_narrow_on_offset_dynamic_index_does_not_narrow() {
    // `arr[i] is ChildA` with a variable index has no stable path —
    // `arr[i]` reads inside the body still see the declared element
    // type, because `i` could differ between guard and use.
    let src = "\
abstract type Base { name: String; }
type ChildA extends Base { extra: int; }
type ChildB extends Base { tag: String; }
fn takesChildA(c: ChildA) {}
fn main(i: int) {
    var arr: Array<Base> = Array<Base> { ChildA { name: \"x\", extra: 1 } };
    if (arr[i] is ChildA) {
        takesChildA(arr[i]);
    }
}
";
    let (uri, pa) = analyze_with_std(src);
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        !diags.is_empty(),
        "expected dynamic-index offsets to skip narrowing, got: {diags:?}"
    );
}

#[test]
fn is_narrow_on_offset_under_member_chain() {
    // `obj.arr[0] is Child` should narrow `obj.arr[0]` so the body
    // sees `obj.arr[0].extra` resolve against `Child`. This is the
    // exact ifc_test shape: `e.attrs[0] is IfcFloatAttr` →
    // `e.attrs[0].value` should resolve on `IfcFloatAttr`.
    let src = "\
abstract type Base { name: String; }
type Child extends Base { extra: int; }
type Holder { arr: Array<Base>; }
fn main() {
    var h = Holder { arr: Array<Base> { Child { name: \"x\", extra: 1 } } };
    if (h.arr[0] is Child) {
        var _x = h.arr[0].extra;
    }
}
";
    let (uri, pa) = analyze_with_std(src);
    let diags = unknown_member_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected member access under chain narrow, got: {diags:?}"
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
