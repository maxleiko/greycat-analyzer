//! Object-construction shape enforcement.
//!
//! GreyCat's `T { … }` carries implicit shape rules: only `Array`
//! accepts positional initializers of any arity; `node` accepts at
//! most one; every other type (user-defined types, `Map`, etc.) must
//! use the named form. The runtime rejects the wrong shape; the
//! analyzer mirrors that here via `collect_object_construction_diags`.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

/// Common v8-shape synthetic-stdlib body. Wrapped in a macro so the
/// v7 fixture can fold it into a `concat!()` literal at compile time
/// rather than allocating at runtime.
macro_rules! std_core_body {
    () => {
        "native type any {}\n\
         native type null {}\n\
         native type bool {}\n\
         native type int {}\n\
         native type float {}\n\
         native type String {}\n\
         native type char {}\n\
         native type Array<T> {}\n\
         native type Map<K, V> {}\n\
         native type node<T> {}\n"
    };
}

/// v8 stdlib surface: the well-known slots `array_decl` / `node_decl`
/// / etc. populate; the v7 `t2` / `t2f` / … / `str` slots stay
/// `None`, so the v7-only checks are inert.
const STD_CORE: &str = std_core_body!();

/// v7 stdlib surface: the v8 body plus the seven fixed-shape tuple
/// natives the v7-only rules dispatch against.
const STD_CORE_V7: &str = concat!(
    std_core_body!(),
    "native type t2 {}\n\
     native type t2f {}\n\
     native type t3 {}\n\
     native type t3f {}\n\
     native type t4 {}\n\
     native type t4f {}\n\
     native type str {}\n",
);

fn analyze_with(std_src: &str, user_src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, std_src, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), user_src, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

fn analyze(user_src: &str) -> (Uri, ProjectAnalysis) {
    analyze_with(STD_CORE, user_src)
}

fn analyze_v7(user_src: &str) -> (Uri, ProjectAnalysis) {
    analyze_with(STD_CORE_V7, user_src)
}

fn codes(pa: &ProjectAnalysis, uri: &Uri) -> Vec<&'static str> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d.code,
                "positional-object-init"
                    | "node-init-arity"
                    | "node-tag-no-init"
                    | "fixed-tuple-arity"
                    | "fixed-tuple-element-type"
            )
        })
        .map(|d| d.code)
        .collect()
}

#[test]
fn positional_init_on_user_type_rejected() {
    let (uri, pa) = analyze(
        "type Person { child: Child; }\n\
         type Child { name: String; }\n\
         fn main() { var _ = Person { Child }; }\n",
    );
    let cs = codes(&pa, &uri);
    assert_eq!(
        cs,
        vec!["positional-object-init"],
        "expected one positional-object-init diag"
    );
    let m = pa.module(&uri).unwrap();
    let msg = &m
        .analysis
        .diagnostics
        .iter()
        .find(|d| d.code == "positional-object-init")
        .unwrap()
        .message;
    assert!(msg.contains("Person"), "msg should name Person: {msg}");
    assert!(
        msg.contains("named form"),
        "msg should suggest named form: {msg}"
    );
}

#[test]
fn positional_init_on_map_rejected() {
    let (uri, pa) = analyze("fn main() { var _ = Map { 42 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["positional-object-init"]);
}

#[test]
fn named_init_on_user_type_no_diag() {
    let (uri, pa) = analyze(
        "type Person { child: Child; }\n\
         type Child { name: String; }\n\
         fn main() { var _ = Person { child: Child { name: \"x\" } }; }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn empty_braces_no_diag() {
    // `T {}` is always valid (default-init), regardless of T.
    let (uri, pa) = analyze(
        "type Person {}\n\
         fn main() {\n\
             var _a = Person {};\n\
             var _b = Map {};\n\
             var _c = Array {};\n\
             var _d = node {};\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn array_positional_any_arity_no_diag() {
    let (uri, pa) = analyze(
        "fn main() {\n\
             var _a = Array { 1 };\n\
             var _b = Array { 1, 2, 3 };\n\
             var _c = Array<int> { 4, 5 };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn node_zero_or_one_positional_no_diag() {
    let (uri, pa) = analyze(
        "type Person {}\n\
         fn main() {\n\
             var _a = node {};\n\
             var _b = node { 42 };\n\
             var _c = node { Person };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn node_more_than_one_positional_rejected() {
    let (uri, pa) = analyze("fn main() { var _ = node { 1, 2 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["node-init-arity"]);
}

#[test]
fn map_named_with_string_keys_no_diag() {
    // `Map { "k": v }` parses via `object_fields` (the grammar's
    // `name` slot is `_expr`, so string literals are valid names).
    // This pass should not fire because any field with `name: Some(_)`
    // is the named form.
    let (uri, pa) = analyze("fn main() { var _ = Map { \"a\": 1, \"b\": 2 }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

// ---------------------------------------------------------------------------
// v7 fixed-shape tuple natives: t2 / t2f / t3 / t3f / t4 / t4f / str.
// ---------------------------------------------------------------------------

#[test]
fn v7_t2_correct_shape_no_diag() {
    let (uri, pa) = analyze_v7("fn main() { var _ = t2 { 1, 2 }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_t2f_correct_shape_no_diag() {
    let (uri, pa) = analyze_v7("fn main() { var _ = t2f { 1.0, 2.5 }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_t3_t4_correct_shape_no_diag() {
    let (uri, pa) = analyze_v7(
        "fn main() {\n\
             var _a = t3 { 1, 2, 3 };\n\
             var _b = t3f { 1.0, 2.0, 3.0 };\n\
             var _c = t4 { 1, 2, 3, 4 };\n\
             var _d = t4f { 1.0, 2.0, 3.0, 4.0 };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_str_one_string_arg_no_diag() {
    let (uri, pa) = analyze_v7("fn main() { var _ = str { \"hello\" }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_t2_wrong_arity_rejected() {
    let (uri, pa) = analyze_v7("fn main() { var _ = t2 { 1 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["fixed-tuple-arity"]);
}

#[test]
fn v7_t3_too_many_rejected() {
    let (uri, pa) = analyze_v7("fn main() { var _ = t3 { 1, 2, 3, 4 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["fixed-tuple-arity"]);
}

#[test]
fn v7_str_wrong_arity_rejected() {
    let (uri, pa) = analyze_v7("fn main() { var _ = str { \"a\", \"b\" }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["fixed-tuple-arity"]);
}

#[test]
fn v7_t2_float_in_int_slot_rejected() {
    let (uri, pa) = analyze_v7("fn main() { var _ = t2 { 1.0, 2.0 }; }\n");
    let cs = codes(&pa, &uri);
    // Two element-type diags — one per offending arg.
    assert_eq!(
        cs,
        vec!["fixed-tuple-element-type", "fixed-tuple-element-type"]
    );
}

#[test]
fn v7_t2f_int_coerces_to_float_no_diag() {
    // Float-tuple variants asymmetrically accept `int` — the
    // runtime coerces `int → float` at construction time. Verified
    // against `greycat run` v7.8 (`t2f { 1, 2 }` outputs
    // `t2f{1.0, 2.0}`).
    let (uri, pa) = analyze_v7("fn main() { var _ = t2f { 1, 2 }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_t2f_int_var_coerces_to_float_no_diag() {
    // Coercion isn't literal-only: `t2f { intVar, intVar }` also
    // works at runtime, so the analyzer must allow it too.
    let (uri, pa) = analyze_v7(
        "fn main() {\n\
             var a = 1;\n\
             var b = 2;\n\
             var _ = t2f { a, b };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no diag expected, got: {cs:?}");
}

#[test]
fn v7_t2_float_var_in_int_slot_rejected() {
    // The reverse direction is asymmetric: `t2` (int) rejects
    // float values even as variables — runtime errors with
    // "t2 requires int constructor parameters".
    let (uri, pa) = analyze_v7(
        "fn main() {\n\
             var a: float = 1.0;\n\
             var b: float = 2.0;\n\
             var _ = t2 { a, b };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert_eq!(
        cs,
        vec!["fixed-tuple-element-type", "fixed-tuple-element-type"]
    );
}

#[test]
fn v7_str_wrong_element_type_rejected() {
    let (uri, pa) = analyze_v7("fn main() { var _ = str { 42 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["fixed-tuple-element-type"]);
}

#[test]
fn v7_arity_diag_suppresses_element_type_diag() {
    // When arity is wrong we don't pile per-element-type errors on
    // top — the user has one problem (count), not N+1.
    let (uri, pa) = analyze_v7("fn main() { var _ = t3 { 1.0, 2.0 }; }\n");
    let cs = codes(&pa, &uri);
    assert_eq!(cs, vec!["fixed-tuple-arity"]);
}

#[test]
fn v8_stdlib_does_not_trigger_v7_rules() {
    // `t2` is not a known type when the v8 fixture is loaded —
    // the head resolves to nothing and the positional-object-init
    // fallback also can't fire (the type-ident doesn't resolve to
    // anything). Make sure neither v7 code surfaces.
    let (uri, pa) = analyze("fn main() { var _ = t2 { 1, 2 }; }\n");
    let cs = codes(&pa, &uri);
    assert!(
        !cs.contains(&"fixed-tuple-arity") && !cs.contains(&"fixed-tuple-element-type"),
        "v7-only diags must not fire on v8 stdlib, got: {cs:?}"
    );
}

// ---------------------------------------------------------------------------
// `Map { k: v }` — keys are value expressions, not field names. The
// `{ <expr>: <expr> }` form parses as the named `object_fields` body, so
// it must NOT trip the positional-object-init rule even when the keys
// aren't bare idents.
// ---------------------------------------------------------------------------

/// Every diagnostic message on the module (for asserting on
/// non-construction diags like `unknown-field`).
fn all_messages(pa: &ProjectAnalysis, uri: &Uri) -> Vec<String> {
    let m = pa.module(uri).expect("module");
    m.analysis
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

#[test]
fn map_enum_variant_keys_no_diag() {
    // The reported bug: `Map<Level, int> { Level::Low: 0, … }` is a
    // valid named construction (enum-variant keys are values), not a
    // positional init.
    let (uri, pa) = analyze(
        "enum Level { Low, Medium, High }\n\
         fn main() {\n\
             var _ = Map<Level, int> { Level::Low: 0, Level::Medium: 1, Level::High: 2 };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no construction diag expected, got: {cs:?}");
}

#[test]
fn map_number_keys_no_diag() {
    let (uri, pa) = analyze("fn main() { var _ = Map<int, bool> { 0: false, 1: true }; }\n");
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no construction diag expected, got: {cs:?}");
}

#[test]
fn map_variable_key_resolves_as_value() {
    // A bare-ident key binds to the local in scope (verified against
    // `greycat run`: the key takes the variable's *value*). So it must
    // resolve cleanly — no unresolved-name, no construction diag.
    let (uri, pa) = analyze(
        "fn main() {\n\
             var k = 7;\n\
             var _ = Map<int, int> { k: 1 };\n\
         }\n",
    );
    let cs = codes(&pa, &uri);
    assert!(cs.is_empty(), "no construction diag expected, got: {cs:?}");
    let msgs = all_messages(&pa, &uri);
    assert!(
        !msgs.iter().any(|m| m.contains("unresolved")),
        "the variable key `k` must resolve as a value: {msgs:?}"
    );
}

// ---------------------------------------------------------------------------
// String-literal field names — `type Foo { "hello world": int; }` is
// valid, and so is constructing it with the same quoted key. Verified
// round-tripping on the v8 runtime.
// ---------------------------------------------------------------------------

#[test]
fn string_literal_field_name_round_trips() {
    let (uri, pa) = analyze(
        "type Foo { \"hello world\": int; }\n\
         fn main() { var _ = Foo { \"hello world\": 42 }; }\n",
    );
    let msgs = all_messages(&pa, &uri);
    assert!(
        !msgs.iter().any(|m| m.contains("unknown field")),
        "string-literal field name must resolve: {msgs:?}"
    );
}

#[test]
fn wrong_string_literal_field_name_flags_unknown() {
    let (uri, pa) = analyze(
        "type Foo { \"hello world\": int; }\n\
         fn main() { var _ = Foo { \"nope\": 42 }; }\n",
    );
    let msgs = all_messages(&pa, &uri);
    assert!(
        msgs.iter().any(|m| m.contains("unknown field")),
        "a wrong string key must flag unknown-field: {msgs:?}"
    );
}

#[test]
fn non_ident_key_on_user_type_flags() {
    // A non-Map user type's keys must be field names; a value-expr
    // key (enum variant) is a malformed construction the runtime
    // rejects as "unresolved field".
    let (uri, pa) = analyze(
        "enum Level { Low }\n\
         type Foo { a: int; }\n\
         fn main() { var _ = Foo { Level::Low: 0 }; }\n",
    );
    let msgs = all_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m.contains("must be an identifier or string literal")),
        "non-ident key on a user type must flag: {msgs:?}"
    );
}

/// `nodeList` / `nodeTime` / `nodeGeo` / `nodeIndex` take no initializer
/// at all (unlike `node`, which accepts one). Any positional content is
/// a runtime error; the empty default-init `T {}` stays valid.
#[test]
fn node_collection_tags_reject_any_initializer() {
    const STD: &str = concat!(
        std_core_body!(),
        "native type nodeTime<T> {}\n\
         native type nodeIndex<K, V> {}\n\
         native type nodeList<T> {}\n\
         native type nodeGeo<T> {}\n",
    );
    let (uri, pa) = analyze_with(
        STD,
        "fn main() {\n\
         var a = nodeList<int> { 1 };\n\
         var b = nodeTime<int> { 1 };\n\
         var c = nodeGeo<int> { 1 };\n\
         var d = nodeIndex<int, int> { 1 };\n\
         var ok = nodeList<int> {};\n\
         }\n",
    );
    let got = codes(&pa, &uri);
    assert_eq!(
        got,
        vec![
            "node-tag-no-init",
            "node-tag-no-init",
            "node-tag-no-init",
            "node-tag-no-init"
        ],
        "each non-empty node-collection init warns once; the empty one is valid: {got:?}"
    );
    let msgs = all_messages(&pa, &uri);
    assert!(
        msgs.iter()
            .any(|m| m == "`nodeList` does not accept any initializer"),
        "message should name the head type and say no initializer: {msgs:?}"
    );
}
