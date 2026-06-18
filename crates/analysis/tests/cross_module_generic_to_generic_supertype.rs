//! Regression: a generic `Sub<T> extends Base<F<T>>` (where `F` is
//! another user-defined generic) must be assignable to a parameter
//! typed as the substituted parent. The runtime accepts this; the
//! analyzer used to reject it because the `Generic → Generic` arm in
//! `is_assignable_to_with_index` only honored the same-decl case and
//! the node-tag bivariance rule — there was no walk of the source
//! decl's `supertype_ty` chain when the head decls differed.
//!
//! Real-world incidence (stdlib): `MultiQuantizer<T> extends
//! Quantizer<Array<T>>`; user code passing a `MultiQuantizer<int>` to
//! a slot of type `Quantizer<Array<int>>` saw a spurious type-mismatch
//! diagnostic.
//!
//! The fixture uses a user-defined `Wrap<T>` container instead of the
//! runtime `Array<T>` so we don't depend on stdlib seeding in unit
//! tests (built-in `Array` resolves to `Unresolved` without a loaded
//! stdlib, and `Unresolved` is permissive on both sides — which would
//! make the negative test trivially pass).

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
fn generic_subtype_assignable_to_substituted_generic_supertype() {
    // `Sub<T> extends Base<Wrap<T>>`; call site passes `Sub<int>` to
    // a parameter typed `Base<Wrap<int>>`. Substitution along the
    // chain produces `Base<Wrap<int>>` which matches the target.
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<Wrap<int>>) {}\n\
         fn caller(s: Sub<int>) {\n    take(s);\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Sub<int> should be assignable to Base<Wrap<int>>, got: {:#?}",
        diags
    );
}

#[test]
fn generic_subtype_assignable_to_wildcard_generic_supertype() {
    // `Sub<T> extends Base<Wrap<T>>`; consume site expects
    // `Base<any?>`. core's all-Any wildcard rule fires on the
    // substituted shape (`Base<Wrap<int>>` vs `Base<any?>`).
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<any?>) {}\n\
         fn caller(s: Sub<int>) {\n    take(s);\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        diags.is_empty(),
        "Sub<int> should be assignable to Base<any?>, got: {:#?}",
        diags
    );
}

#[test]
fn generic_subtype_unrelated_concrete_arg_still_rejected() {
    // `Sub<T> extends Base<Wrap<T>>`; consume site expects
    // `Base<Wrap<String>>`. Substitution produces `Base<Wrap<int>>`,
    // which is not assignable to `Base<Wrap<String>>` (no wildcard).
    let mut mgr = SourceManager::new();
    add(&mut mgr, "/proj/src/types.gcl", TYPES_SRC);
    let main_uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "fn take(b: Base<Wrap<String>>) {}\n\
         fn caller(s: Sub<int>) {\n    take(s);\n}\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &main_uri);
    assert!(
        !diags.is_empty(),
        "Sub<int> : Base<Wrap<int>> must NOT be assignable to Base<Wrap<String>>; \
         analyzer let it through silently",
    );
}
