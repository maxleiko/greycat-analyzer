//! Type-relation check for object-construction fields: a supplied
//! value must be assignable to the attr's declared type after
//! substituting the object expression's generic args (and any
//! `extends Base<X>` along the supertype chain).
//!
//! Lives in `validate_type_relations` per the architectural rule that
//! every type-relation diagnostic flows through that pass. The
//! structural completeness check (`missing-required-fields` /
//! `unknown-field`) is a separate concern that fires from the
//! analyzer; this check is purely about value vs. declared types.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn add(mgr: &mut SourceManager, path: &str, src: &str) -> Uri {
    let uri = Uri::from_str(&format!("file://{path}")).unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    uri
}

fn mismatch_diags(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| d.code == "field-type-mismatch")
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn wrong_value_type_for_simple_attr_rejected() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Foo { a: int; }\n\
         fn main() { var _ = Foo { a: \"oops\" }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = mismatch_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one diag, got: {diags:#?}");
    assert!(
        diags[0].contains("field `a:"),
        "should name `a`: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("String"),
        "should mention String: {}",
        diags[0]
    );
    assert!(diags[0].contains("int"), "should mention int: {}", diags[0]);
}

#[test]
fn correct_value_type_no_diag() {
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Foo { a: int; }\n\
         fn main() { var _ = Foo { a: 1 }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = mismatch_diags(&pa, &uri);
    assert!(diags.is_empty(), "no diag expected, got: {diags:#?}");
}

#[test]
fn substituted_generic_attr_checks_against_concrete() {
    // Stdlib-faithful shape: `MultiQuantizer<T> extends Quantizer<Wrap<T>>`
    // with `quantizers: Wrap<Quantizer<T>>`. `MultiQuantizer<int> { quantizers: false }`
    // must compare `bool` against the substituted `Wrap<Quantizer<int>>`
    // and reject — runtime regression from kopr. Uses a user-defined
    // `Wrap<T>` instead of stdlib `Array<T>` because unit tests don't
    // seed `Array` (it'd lower to `Unresolved`, which is permissive on
    // both sides and would mask the negative case).
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Wrap<W> { inner: W; }\n\
         abstract type Quantizer<T> {}\n\
         type MultiQuantizer<T> extends Quantizer<Wrap<T>> {\n\
             quantizers: Wrap<Quantizer<T>>;\n\
         }\n\
         fn main() { var _ = MultiQuantizer<int> { quantizers: false }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = mismatch_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected one diag, got: {diags:#?}");
    assert!(
        diags[0].contains("field `quantizers:"),
        "should name the field: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("bool"),
        "should mention bool value: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("Quantizer<int>"),
        "T should be substituted: {}",
        diags[0]
    );
}

#[test]
fn inherited_attr_substitutes_through_extends() {
    // `Sub` is non-generic but extends `Base<int>`. The inherited
    // `val: T` resolves to `int` at the Sub level, so
    // `Sub { val: 1 }` is fine and `Sub { val: "oops" }` is rejected
    // against `val: int`, NOT against `val: T` (which would always
    // reject and was the bug surfaced when the chain walk forgot to
    // substitute parent generics).
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "abstract type Base<T> { val: T; }\n\
         type Sub extends Base<int> {}\n\
         fn ok() { var _ = Sub { val: 1 }; }\n\
         fn bad() { var _ = Sub { val: \"oops\" }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = mismatch_diags(&pa, &uri);
    assert_eq!(diags.len(), 1, "expected exactly one diag, got: {diags:#?}");
    assert!(
        diags[0].contains("field `val:"),
        "should name `val`: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("val: int"),
        "should substitute T → int: {}",
        diags[0]
    );
}

#[test]
fn inherited_concrete_attr_checks_directly() {
    // Inherited non-generic attr (`name: String` on parent) — `String`
    // doesn't need substitution. Make sure the chain walk doesn't
    // mangle non-substituting hops.
    let mut mgr = SourceManager::new();
    let uri = add(
        &mut mgr,
        "/proj/src/main.gcl",
        "type Animal { name: String; }\n\
         type Dog extends Animal { breed: String; }\n\
         fn main() { var _ = Dog { name: 42, breed: \"lab\" }; }\n",
    );
    let pa = ProjectAnalysis::analyze(&mgr);
    let diags = mismatch_diags(&pa, &uri);
    assert_eq!(
        diags.len(),
        1,
        "expected one diag for `name`, got: {diags:#?}"
    );
    assert!(
        diags[0].contains("field `name:"),
        "should name `name`: {}",
        diags[0]
    );
    assert!(
        diags[0].contains("int"),
        "should mention int value: {}",
        diags[0]
    );
}
