//! Regression: `is_castable_with_index`'s `Generic → Generic` arm used
//! to ignore generic args entirely — `bidi_inherits` only checked
//! decl-handle subtyping, so obviously-wrong casts like
//! `MultiQuantizer<int> as Quantizer<Array<String>>` slid through.
//!
//! The GreyCat runtime drops `as` casts entirely; the analyzer is the
//! only safety net, so cast strictness on generic args must match
//! assignability's. After the tightening (which reuses
//! [`walk_substituted_supertype_chain`] shared with assignability),
//! cross-decl generic casts walk the more-specific side's chain with
//! its actual args substituted, then compare each hop against the
//! other side via core `is_castable` (invariant on args).
//!
//! Uses a user-defined `Wrap<T>` container instead of stdlib `Array<T>`
//! so unit tests don't depend on stdlib seeding (built-in `Array`
//! resolves to `Unresolved` without a loaded stdlib, which is
//! permissive on both sides and would mask the negative cases).

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn cast_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("cannot cast"))
        .map(|d| d.message.clone())
        .collect()
}

const TYPES_SRC: &str = "\
type Wrap<W> {\n\
    inner: W;\n\
}\n\
abstract type Base<U> {\n\
    val: U;\n\
}\n\
type Sub<T> extends Base<Wrap<T>> {\n\
    items: Wrap<T>;\n\
}\n\
";

#[test]
fn upcast_generic_to_substituted_supertype_accepted() {
    // `Sub<int> as Base<Wrap<int>>` — substituted parent shape matches
    // the cast target. Should be accepted (it's just an explicit
    // upcast).
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(s: Sub<int>) {\n    var b = s as Base<Wrap<int>>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Sub<int> as Base<Wrap<int>> should be accepted, got: {:#?}",
        diags
    );
}

#[test]
fn upcast_generic_to_wrong_arg_supertype_rejected() {
    // `Sub<int> as Base<Wrap<String>>` — substituted parent shape is
    // `Base<Wrap<int>>`, which doesn't match the target. The runtime
    // won't catch this; the analyzer must.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(s: Sub<int>) {\n    var b = s as Base<Wrap<String>>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Sub<int> as Base<Wrap<String>> must NOT be accepted; analyzer let it through silently",
    );
}

#[test]
fn downcast_generic_supertype_to_subtype_with_matching_args_accepted() {
    // `Base<Wrap<int>> as Sub<int>` — downcast direction. Walk Sub's
    // chain (Sub is more specific) with [int]; substituted hop is
    // `Base<Wrap<int>>`, matches source. Accepted as a downcast.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(b: Base<Wrap<int>>) {\n    var s = b as Sub<int>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Base<Wrap<int>> as Sub<int> should be accepted, got: {:#?}",
        diags
    );
}

#[test]
fn downcast_generic_supertype_to_subtype_with_wrong_args_rejected() {
    // `Base<Wrap<String>> as Sub<int>` — downcast direction with
    // mismatched arg. Walk Sub's chain with [int]; hop is
    // `Base<Wrap<int>>`, doesn't match `Base<Wrap<String>>`. Reject.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(b: Base<Wrap<String>>) {\n    var s = b as Sub<int>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Base<Wrap<String>> as Sub<int> must NOT be accepted",
    );
}

#[test]
fn same_decl_mismatched_concrete_args_rejected() {
    // `Wrap<int> as Wrap<String>` — same decl, mismatched concrete
    // args. The previous `bidi_inherits` shortcut accepted this
    // because the decls trivially share an inheritance relation
    // (subtype-of self). Now: core's `is_castable` already rejected
    // (per-arg invariance), and the wrapper no longer relaxes
    // same-decl casts beyond node-tag bivariance.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(w: Wrap<int>) {\n    var v = w as Wrap<String>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Wrap<int> as Wrap<String> must NOT be accepted",
    );
}

#[test]
fn unrelated_generic_decls_rejected() {
    // Two unrelated generic decls; `bidi_inherits` would have said
    // false (handle-walk finds no chain). The walker also returns
    // false (no chain to walk in either direction). Reject.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    add(
        &mut mgr,
        "/proj/src/other.gcl",
        "type Other<X> {\n    z: X;\n}\n",
    );
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn caller(w: Wrap<int>) {\n    var o = w as Other<int>;\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = cast_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Wrap<int> as Other<int> must NOT be accepted (unrelated decls)",
    );
}
