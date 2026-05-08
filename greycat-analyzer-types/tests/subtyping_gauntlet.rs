//! Subtyping fixtures gauntlet (P12.4).
//!
//! Each rule in this file is paired with a runtime probe — an
//! `Allowed` / `Rejected` annotation captured by running the
//! corresponding source through `greycat run` (the GreyCat compiler /
//! runtime, our true oracle). When the TS reference checker
//! (`packages/lang/src/analysis/typesystem.test.ts`) and the runtime
//! disagree, we follow the runtime — see
//! `feedback_runtime_is_oracle.md` and CLAUDE.md's verification order.
//!
//! Probes were captured via:
//! ```sh
//! cat > /tmp/p124-tests/project.gcl << EOF
//! @library("std", "8.0.269-dev");
//! fn main() { var v: <SRC> = <INIT>; take(v); }
//! fn take(_: <TGT>) {}
//! EOF
//! cd /tmp/p124-tests && /home/leiko/.greycat/bin/greycat run
//! ```
//! against the live stdlib. Tests below assert the runtime outcome.

use greycat_analyzer_types::{GenericOwner, Primitive, TypeArena, is_assignable_to, is_castable};

fn arena() -> TypeArena {
    TypeArena::new()
}

// =============================================================================
// Primitive widening (none — runtime rejects every cross-primitive flow)
// =============================================================================

#[test]
fn rt_int_to_int_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    assert!(is_assignable_to(&a, i, i));
}

#[test]
fn rt_int_to_float_rejected() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: float)` is
    // REJECTED. TS reference says allowed; we follow the runtime.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let f = a.primitive(Primitive::Float);
    assert!(!is_assignable_to(&a, i, f));
}

#[test]
fn rt_float_to_int_rejected() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let f = a.primitive(Primitive::Float);
    assert!(!is_assignable_to(&a, f, i));
}

#[test]
fn rt_char_to_int_rejected() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let c = a.primitive(Primitive::Char);
    assert!(!is_assignable_to(&a, c, i));
}

#[test]
fn rt_string_to_char_rejected() {
    let mut a = arena();
    let s = a.primitive(Primitive::String);
    let c = a.primitive(Primitive::Char);
    assert!(!is_assignable_to(&a, s, c));
}

// =============================================================================
// any top / never bottom
// =============================================================================

#[test]
fn rt_int_to_any_allowed() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: any)` is
    // ALLOWED.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let any = a.any();
    assert!(is_assignable_to(&a, i, any));
}

#[test]
fn rt_any_to_int_rejected() {
    // Runtime: `var a: any = 1; take(a)` against `take(_: int)` is
    // REJECTED. `any` is the top type — values flow *into* it, not
    // *out of* it without a cast.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let any = a.any();
    assert!(!is_assignable_to(&a, any, i));
}

#[test]
fn rt_string_to_any_allowed() {
    let mut a = arena();
    let s = a.primitive(Primitive::String);
    let any = a.any();
    assert!(is_assignable_to(&a, s, any));
}

// =============================================================================
// Nullable widening (T → T?, T? → T fails)
// =============================================================================

#[test]
fn rt_int_to_nullable_int_allowed() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: int?)` is
    // ALLOWED.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    assert!(is_assignable_to(&a, i, i_q));
}

#[test]
fn rt_nullable_int_to_int_rejected_when_null() {
    // Runtime: `take(null)` for `take(_: int)` is REJECTED at the
    // dynamic call site. Statically, `int?` does not flow to `int`.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    assert!(!is_assignable_to(&a, i_q, i));
}

#[test]
fn rt_null_to_nullable_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    let n = a.null();
    assert!(is_assignable_to(&a, n, i_q));
    assert!(!is_assignable_to(&a, n, i));
}

// =============================================================================
// Generic args — invariant in concrete types, but `any` is the escape
// =============================================================================

#[test]
fn rt_array_int_to_array_int_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let arr_i = a.generic("Array", vec![i]);
    assert!(is_assignable_to(&a, arr_i, arr_i));
}

#[test]
fn rt_array_int_to_array_float_rejected() {
    // Runtime: `var v: Array<int> = [1]; take(v)` against
    // `take(_: Array<float>)` is REJECTED.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let f = a.primitive(Primitive::Float);
    let arr_i = a.generic("Array", vec![i]);
    let arr_f = a.generic("Array", vec![f]);
    assert!(!is_assignable_to(&a, arr_i, arr_f));
    assert!(!is_assignable_to(&a, arr_f, arr_i));
}

#[test]
fn rt_array_int_to_array_nullable_int_rejected() {
    // Runtime: `Array<int>` → `Array<int?>` is REJECTED. Even though
    // `int → int?` is allowed at the bare-primitive level, the
    // generic-arg position stays invariant. KNOWN GAP: our current
    // `is_assignable_to` strictly compares TypeIds, which matches the
    // runtime here. (Marked separately so the rule is documented.)
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    let arr_i = a.generic("Array", vec![i]);
    let arr_iq = a.generic("Array", vec![i_q]);
    assert!(!is_assignable_to(&a, arr_i, arr_iq));
}

// =============================================================================
// Tuple element-wise (identity on each position)
// =============================================================================

#[test]
fn rt_tuple_identity_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let s = a.primitive(Primitive::String);
    let t1 = a.tuple(vec![i, s]);
    let t2 = a.tuple(vec![i, s]);
    assert!(is_assignable_to(&a, t1, t2));
}

#[test]
fn rt_tuple_element_mismatch_rejected() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let s = a.primitive(Primitive::String);
    let f = a.primitive(Primitive::Float);
    let t1 = a.tuple(vec![i, s]);
    let t2 = a.tuple(vec![f, s]);
    assert!(!is_assignable_to(&a, t1, t2));
}

// =============================================================================
// Cast rules (P12.3) — asymmetric promotions
// =============================================================================

#[test]
fn rt_cast_int_to_node_tags_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    for tag in ["node", "nodeTime", "nodeList", "nodeIndex", "nodeGeo"] {
        let tagged = a.named(tag);
        assert!(is_castable(&a, i, tagged), "int as {tag} should be allowed",);
    }
}

#[test]
fn rt_cast_node_tags_to_int_allowed() {
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    for tag in ["node", "nodeTime", "nodeList", "nodeIndex", "nodeGeo"] {
        let tagged = a.named(tag);
        assert!(is_castable(&a, tagged, i), "{tag} as int should be allowed",);
    }
}

#[test]
fn rt_cast_string_to_int_rejected() {
    let mut a = arena();
    let s = a.primitive(Primitive::String);
    let i = a.primitive(Primitive::Int);
    assert!(!is_castable(&a, s, i));
}

#[test]
fn rt_cast_char_to_int_allowed() {
    let mut a = arena();
    let c = a.primitive(Primitive::Char);
    let i = a.primitive(Primitive::Int);
    assert!(is_castable(&a, c, i));
}

// =============================================================================
// GenericParam unification (P7.4 / P12.1)
// =============================================================================

#[test]
fn rt_generic_param_substitution_through_inference_table() {
    // Sanity-check that the GenericParam constructor actually round-trips
    // through `is_assignable_to` semantics: a bare GenericParam is
    // assignable only to itself.
    let mut a = arena();
    let t1 = a.generic_param("T", GenericOwner::Function("f".into()));
    let t2 = a.generic_param("T", GenericOwner::Function("f".into()));
    let u = a.generic_param("U", GenericOwner::Function("f".into()));
    assert_eq!(t1, t2, "interning should collapse identical GenericParams");
    assert!(is_assignable_to(&a, t1, t2));
    assert!(!is_assignable_to(&a, t1, u));
}
