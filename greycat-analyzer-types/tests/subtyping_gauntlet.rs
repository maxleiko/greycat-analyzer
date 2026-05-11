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
fn rt_any_to_int_allowed() {
    // **P20.1** — Runtime: `var a: any = 1; take(a)` against
    // `take(_: int)` *compiles* and *runs* successfully (the runtime
    // accepts `any → T` at compile time and defers the dynamic type
    // check to call time; only `var a: any = "x"; take(a)` fails —
    // and that failure is an `Error` raised at runtime, not a
    // compile-time rejection). The earlier "rejected" capture
    // conflated runtime *dynamic* dispatch failures with compile-time
    // assignability; verified against `greycat run` 8.0.269-dev.
    // `is_assignable_to` is a compile-time relation, so `any → T`
    // must pass — `any` is *both* top and bottom in the lattice.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let any = a.any();
    assert!(is_assignable_to(&a, any, i));
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

// =============================================================================
// Node-tag inner-arg bivariance (verified against runtime)
// =============================================================================
//
// The GreyCat runtime stores `node` / `nodeTime` / `nodeIndex` /
// `nodeList` / `nodeGeo` as 64-bit handles, so passing
// `nodeTime<float>` where `nodeTime<float?>` is expected (and the
// reverse) is accepted — the inner type doesn't constrain the wire
// representation. Verified per probe:
//
//   type Holder { ts: nodeTime<float>; }
//   fn takesNullable(x: nodeTime<float?>?) {}
//   fn takesNonNullable(x: nodeTime<float?>) {}
//   ...
//
// All directions for the node-tag set pass at runtime. `Array<T>` /
// `Map<K,V>` stay invariant.

#[test]
fn rt_nodetime_t_to_nodetime_nullable_t_allowed() {
    let mut a = arena();
    let f = a.primitive(Primitive::Float);
    let f_q = a.nullable(f);
    let nt_f = a.generic("nodeTime", vec![f]);
    let nt_fq = a.generic("nodeTime", vec![f_q]);
    assert!(is_assignable_to(&a, nt_f, nt_fq));
    assert!(is_assignable_to(&a, nt_fq, nt_f));
}

#[test]
fn rt_nodelist_t_to_nodelist_unrelated_t_allowed() {
    // Runtime probe: `nodeList<node<Foo>>` flows into a
    // `nodeList<node<Bar>>` slot — the wire shape is a node ref and
    // the runtime doesn't constrain it further.
    let mut a = arena();
    let foo = a.named("Foo");
    let bar = a.named("Bar");
    let node_foo = a.generic("node", vec![foo]);
    let node_bar = a.generic("node", vec![bar]);
    let nl_foo = a.generic("nodeList", vec![node_foo]);
    let nl_bar = a.generic("nodeList", vec![node_bar]);
    assert!(is_assignable_to(&a, nl_foo, nl_bar));
}

#[test]
fn rt_nodeindex_args_are_bivariant() {
    let mut a = arena();
    let s = a.primitive(Primitive::String);
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    let ni_si = a.generic("nodeIndex", vec![s, i]);
    let ni_sni = a.generic("nodeIndex", vec![s, i_q]);
    assert!(is_assignable_to(&a, ni_si, ni_sni));
    assert!(is_assignable_to(&a, ni_sni, ni_si));
}

#[test]
fn rt_array_int_to_array_nullable_int_still_rejected() {
    // Regression guard: Array is NOT a node tag — the bivariance
    // relaxation must not leak. `Array<int>` ↔ `Array<int?>` remain
    // invariant per the runtime.
    let mut a = arena();
    let i = a.primitive(Primitive::Int);
    let i_q = a.nullable(i);
    let arr_i = a.generic("Array", vec![i]);
    let arr_iq = a.generic("Array", vec![i_q]);
    assert!(!is_assignable_to(&a, arr_i, arr_iq));
    assert!(!is_assignable_to(&a, arr_iq, arr_i));
}

// =============================================================================
// Raw-form symmetric assignability: Named<N> ↔ Generic<N, any...>
// =============================================================================
//
// `Generic<N, args>` → `Named<N>` (raw type form) was already allowed.
// The symmetric `Named<N>` → `Generic<N, args>` direction now also
// passes when every target arg is `any`, mirroring the runtime
// (`fn takesAnyArgs(refs: Array<nodeIndex<any?, any?>>) {}` accepts a
// `Array<nodeIndex>` argument).

#[test]
fn rt_named_flows_into_generic_with_all_any_args() {
    // `nodeIndex` (raw) → `nodeIndex<any, any>` allowed.
    let mut a = arena();
    let any = a.any();
    let raw = a.named("nodeIndex");
    let generic = a.generic("nodeIndex", vec![any, any]);
    assert!(is_assignable_to(&a, raw, generic));
}

#[test]
fn rt_named_flows_into_generic_with_nullable_any_args() {
    // The relaxation accepts `any` regardless of nullability — the
    // analyzer minted `any?` from `Cx::lower_type_ref` when the source
    // had `any?`, which still matches the rule.
    let mut a = arena();
    let any = a.any();
    let any_q = a.nullable(any);
    let raw = a.named("nodeIndex");
    let generic = a.generic("nodeIndex", vec![any_q, any_q]);
    assert!(is_assignable_to(&a, raw, generic));
}

#[test]
fn rt_array_named_to_array_generic_via_inner_bidirectional() {
    // The original bug shape: outer `Array<...>` invariant check
    // recurses into the inner arg pair `Generic{nodeIndex, [any?,any?]}`
    // ↔ `Named{nodeIndex}`. The forward direction already passed via
    // the existing raw-form rule; this test pins down that the reverse
    // direction (added in commit 3) lets the bidirectional check succeed.
    let mut a = arena();
    let any = a.any();
    let any_q = a.nullable(any);
    let raw = a.named("nodeIndex");
    let generic = a.generic("nodeIndex", vec![any_q, any_q]);
    let arr_raw = a.generic("Array", vec![raw]);
    let arr_generic = a.generic("Array", vec![generic]);
    assert!(is_assignable_to(&a, arr_generic, arr_raw));
    assert!(is_assignable_to(&a, arr_raw, arr_generic));
}

#[test]
fn rt_named_does_not_flow_into_generic_with_concrete_args() {
    // Negative: the rule only triggers when *every* target arg is
    // `any`. A partial-any / concrete-arg shape must stay rejected,
    // matching the runtime.
    let mut a = arena();
    let any = a.any();
    let i = a.primitive(Primitive::Int);
    let raw = a.named("nodeIndex");
    let generic_partial = a.generic("nodeIndex", vec![any, i]);
    let generic_concrete = a.generic("nodeIndex", vec![i, i]);
    assert!(!is_assignable_to(&a, raw, generic_partial));
    assert!(!is_assignable_to(&a, raw, generic_concrete));
}

// =============================================================================
// Named{N} ↔ GenericParam{N} name match
// =============================================================================
//
// The project-pipeline's validation pass lowers TypeRefs *without*
// threading generic scope, so a declared `V?` parameter / return
// lowers to `Named{name:"V"}`. The body walker lowers the same
// source token as `GenericParam{name:"V", owner:Type(...)}`. These
// must compare equal in `is_assignable_to` (or the validation pass
// would surface "value of `V?` not assignable to declared return type
// `V?`" on identical shapes that print the same way).

#[test]
fn rt_named_v_matches_generic_param_v() {
    let mut a = arena();
    let named_v = a.named("V");
    let gp_v = a.generic_param("V", GenericOwner::Type("Foo".into()));
    assert!(is_assignable_to(&a, named_v, gp_v));
    assert!(is_assignable_to(&a, gp_v, named_v));
}

#[test]
fn rt_named_v_and_generic_param_v_compatible_through_nullable() {
    // Same-name compatibility must survive a wrapping nullable on
    // both sides (the body walker emits `nullable(GenericParam{V})`
    // for `V?`, validation emits `nullable(Named{V})`).
    let mut a = arena();
    let named_v = a.named("V");
    let gp_v = a.generic_param("V", GenericOwner::Type("Foo".into()));
    let named_vq = a.nullable(named_v);
    let gp_vq = a.nullable(gp_v);
    assert!(is_assignable_to(&a, named_vq, gp_vq));
    assert!(is_assignable_to(&a, gp_vq, named_vq));
}

#[test]
fn rt_named_v_does_not_match_generic_param_u() {
    // Negative: different names must stay distinct, no matter the
    // owners.
    let mut a = arena();
    let named_v = a.named("V");
    let gp_u = a.generic_param("U", GenericOwner::Type("Foo".into()));
    assert!(!is_assignable_to(&a, named_v, gp_u));
    assert!(!is_assignable_to(&a, gp_u, named_v));
}

#[test]
fn rt_named_v_matches_generic_param_v_inside_outer_container() {
    // The rule has to fire from the recursive arg-comparison inside
    // an outer container. Build `Tuple<Generic{Table, [Named{V}]},
    // MeasureUnit>` ↔ `Tuple<Generic{Table, [GenericParam{V, ...}]},
    // MeasureUnit>` and assert the bidirectional invariance check
    // succeeds.
    let mut a = arena();
    let named_v = a.named("V");
    let gp_v = a.generic_param("V", GenericOwner::Function("f".into()));
    let mu = a.named("MeasureUnit");
    let tbl_named = a.generic("Table", vec![named_v]);
    let tbl_gp = a.generic("Table", vec![gp_v]);
    let tup_named = a.generic("Tuple", vec![tbl_named, mu]);
    let tup_gp = a.generic("Tuple", vec![tbl_gp, mu]);
    assert!(is_assignable_to(&a, tup_named, tup_gp));
    assert!(is_assignable_to(&a, tup_gp, tup_named));
}
