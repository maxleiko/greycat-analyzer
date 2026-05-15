//! Regression test for generic-method-on-generic-receiver
//! substitution at call-arg validation time.
//!
//! `n.set(42)` where `n: node<int?>` calls `node<T>::set(value: T)`.
//! The validator must substitute `T → int?` before comparing the arg
//! `42: int` against the param `value: T`; otherwise it surfaces a
//! spurious `value of type `int` is not assignable to parameter
//! `value: T`` diagnostic on a program the runtime accepts cleanly.
//!
//! The fix lives in `collect_call_arg_diags_split`: when the callee
//! is `Expr::Member` / `Expr::Arrow` on a `TypeKind::Generic` receiver,
//! we look up the receiver type's `Decl::Type` in the foreign module
//! to find its generic params, pair them with the receiver's args,
//! and route the method's declared `TypeRef`s through
//! `read_type_shape_subst` with the resulting map.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze_with_std(user_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    // Minimal std/core fixture: just the `node<T>` decl with the
    // `set(value: T)` method that triggers the substitution path.
    mgr.add_simple(
        std_uri,
        "native type node<T> {\n  native fn set(value: T);\n  native fn resolve(): T;\n}\n",
        "std",
        false,
    );
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

fn assignability_diagnostics(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.message.contains("not assignable to parameter"))
        .map(|d| d.message.clone())
        .collect()
}

/// The exact scenario from the user's `project.gcl`: a modvar
/// receiver with a node-tag generic, calling a method whose declared
/// param type is the generic param `T`. Pre-fix, the validator
/// compared `int` against `T` (literal) and surfaced a false-positive
/// "value of type `int` is not assignable to parameter `value: T`".
#[test]
fn modvar_node_int_q_set_accepts_int_arg() {
    let (uri, pa) = analyze_with_std(
        "var n: node<int?>;\n\
         fn main() {\n\
           n.set(42);\n\
         }\n",
    );
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors on `n.set(42)` with `n: node<int?>`; got: {diags:?}"
    );
}

/// Negative variant: the type-mismatch path still fires when the
/// substituted param doesn't accept the arg. `node<int>` (non-null
/// inner) rejects `"hello"` because the substituted param is `int`,
/// not `T` or `any`. Guards against the fix accidentally turning
/// every method-call validation into a no-op.
#[test]
fn modvar_node_int_set_rejects_string_arg() {
    let (uri, pa) = analyze_with_std(
        "var n: node<int>;\n\
         fn main() {\n\
           n.set(\"hello\");\n\
         }\n",
    );
    let diags = assignability_diagnostics(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected one assignability diag, got: {diags:?}"
    );
    // Message should mention `int` (the substituted param type), not `T`.
    let msg = &diags[0];
    assert!(
        msg.contains(": int") || msg.contains("`int`"),
        "expected substituted param type `int` in message, got: {msg}"
    );
    assert!(
        !msg.contains(": T"),
        "post-substitution message must not surface the unsubstituted param name `T`; got: {msg}"
    );
}

/// Same shape but with a function parameter instead of a modvar
/// (param-receiver vs modvar-receiver). Confirms the fix isn't
/// modvar-specific — any receiver whose settled type is a
/// `Generic` instantiation gets the substitution.
#[test]
fn param_node_int_q_set_accepts_int_arg() {
    let (uri, pa) = analyze_with_std("fn use_node(n: node<int?>) {\n  n.set(42);\n}\n");
    let diags = assignability_diagnostics(&pa, &uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors on param-receiver `n.set(42)`; got: {diags:?}"
    );
}

/// Regression for the raw-form lowering: a function declared to
/// return `Tensor` (no generic args at the use site) used to lower
/// the declared return type and the body's return value as
/// different shapes (`Named{Tensor}` on one side, `Type(handle)` on
/// the other after a partial 35.7 migration), surfacing a false
/// "return value of type `Tensor` is not assignable to declared
/// return type `Tensor`" diagnostic.
///
/// The fix normalizes raw-form generic references at lowering time:
/// `Tensor` ≡ `Tensor<any?, any?>` per language semantics. Both
/// lowering passes produce the same `Generic{Tensor, [any?, any?]}`
/// shape, the existing `(Generic, Generic)` all-any widening rule
/// accepts any concrete instantiation flowing into the raw form,
/// and no diagnostic fires.
#[test]
fn raw_form_generic_return_accepts_specialized_body() {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    mgr.add_simple(
        std_uri,
        "native type Tensor<K, V> {\n  native fn make(): Tensor<K, V>;\n}\n",
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        // Declared return is the raw-form `Tensor`; body produces a
        // specialized `Tensor<int, float>`. Pre-fix this falsely
        // flagged "not assignable".
        "fn build(): Tensor {\n  return Tensor<int, float>::make();\n}\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &user_uri);
    assert!(
        diags.is_empty(),
        "expected raw-form `Tensor` declared return to accept any specialized body; got: {diags:?}"
    );
}

/// Two-param method substitution (`nodeIndex<K, V>` shape). Confirms
/// the fix handles N-ary generics, not just the single-param node case.
#[test]
fn node_index_set_substitutes_both_generics() {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///lib/std/core.gcl").unwrap();
    let user_uri = Uri::from_str("file:///main.gcl").unwrap();
    mgr.add_simple(
        std_uri,
        "native type nodeIndex<K, V> {\n  native fn set(key: K, value: V): V;\n}\n",
        "std",
        false,
    );
    mgr.add_simple(
        user_uri.clone(),
        "var idx: nodeIndex<String, int>;\n\
         fn main() {\n\
           idx.set(\"k\", 42);\n\
         }\n",
        "project",
        false,
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = assignability_diagnostics(&pa, &user_uri);
    assert!(
        diags.is_empty(),
        "expected no assignability errors on `nodeIndex<String, int>::set(\"k\", 42)`; got: {diags:?}"
    );
}
