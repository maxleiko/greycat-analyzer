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

use greycat_analyzer_core::{Builtins, ItemId, SymbolTable, TypeArena};

fn arena() -> TypeArena {
    let mut a = TypeArena::new();
    // Primitives are `Type(core::X)`, minted via `a.builtin(Builtins::INT)`,
    // so the arena needs its canonical builtin identities set. The
    // symbol table is throwaway -- `Builtins` stores `Copy` `ItemId`s.
    a.set_builtins(Builtins::compute(&SymbolTable::new()));
    a
}

// Synthetic decl handles for tests. Every test shares the same fake
// module symbol so distinct names get distinct `ItemId`s — mirrors
// what `ProjectIndex::ingest` would mint at runtime from
// `(module_sym, name_sym)`. Cheap (rodeo dedupes on intern), no
// coordination required across tests.
fn synth_decl(name: &str) -> ItemId {
    static SYMS: std::sync::OnceLock<SymbolTable> = std::sync::OnceLock::new();
    let syms = SYMS.get_or_init(SymbolTable::new);
    ItemId::new(syms.intern("test_mod"), syms.intern(name))
}

// =============================================================================
// Primitive widening (none — runtime rejects every cross-primitive flow)
// =============================================================================

#[test]
fn rt_int_to_int_allowed() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    assert!(a.is_assignable_to(i, i));
}

#[test]
fn rt_int_to_float_rejected() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: float)` is
    // REJECTED. TS reference says allowed; we follow the runtime.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let f = a.builtin(Builtins::FLOAT);
    assert!(!a.is_assignable_to(i, f));
}

#[test]
fn rt_float_to_int_rejected() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let f = a.builtin(Builtins::FLOAT);
    assert!(!a.is_assignable_to(f, i));
}

#[test]
fn rt_char_to_int_rejected() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let c = a.builtin(Builtins::CHAR);
    assert!(!a.is_assignable_to(c, i));
}

#[test]
fn rt_string_to_char_rejected() {
    let mut a = arena();
    let s = a.builtin(Builtins::STRING);
    let c = a.builtin(Builtins::CHAR);
    assert!(!a.is_assignable_to(s, c));
}

// =============================================================================
// any top / never bottom
// =============================================================================

#[test]
fn rt_int_to_any_allowed() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: any)` is
    // ALLOWED.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let any = a.any();
    assert!(a.is_assignable_to(i, any));
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
    let i = a.builtin(Builtins::INT);
    let any = a.any();
    assert!(a.is_assignable_to(any, i));
}

#[test]
fn rt_string_to_any_allowed() {
    let mut a = arena();
    let s = a.builtin(Builtins::STRING);
    let any = a.any();
    assert!(a.is_assignable_to(s, any));
}

// =============================================================================
// Nullable widening (T → T?, T? → T fails)
// =============================================================================

#[test]
fn rt_int_to_nullable_int_allowed() {
    // Runtime: `var i: int = 1; take(i)` against `take(_: int?)` is
    // ALLOWED.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let i_q = a.nullable(i);
    assert!(a.is_assignable_to(i, i_q));
}

#[test]
fn rt_nullable_int_to_int_rejected_when_null() {
    // Runtime: `take(null)` for `take(_: int)` is REJECTED at the
    // dynamic call site. Statically, `int?` does not flow to `int`.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let i_q = a.nullable(i);
    assert!(!a.is_assignable_to(i_q, i));
}

#[test]
fn rt_null_to_nullable_allowed() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let i_q = a.nullable(i);
    let n = a.null();
    assert!(a.is_assignable_to(n, i_q));
    assert!(!a.is_assignable_to(n, i));
}

// =============================================================================
// Generic args — invariant in concrete types, but `any` is the escape
// =============================================================================

#[test]
fn rt_array_int_to_array_int_allowed() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let arr_i = a.alloc_generic(synth_decl("Array"), vec![i]);
    assert!(a.is_assignable_to(arr_i, arr_i));
}

#[test]
fn rt_array_int_to_array_float_rejected() {
    // Runtime: `var v: Array<int> = [1]; take(v)` against
    // `take(_: Array<float>)` is REJECTED.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let f = a.builtin(Builtins::FLOAT);
    let arr_i = a.alloc_generic(synth_decl("Array"), vec![i]);
    let arr_f = a.alloc_generic(synth_decl("Array"), vec![f]);
    assert!(!a.is_assignable_to(arr_i, arr_f));
    assert!(!a.is_assignable_to(arr_f, arr_i));
}

#[test]
fn rt_array_int_to_array_nullable_int_rejected() {
    // Runtime: `Array<int>` → `Array<int?>` is REJECTED. Even though
    // `int → int?` is allowed at the bare-primitive level, the
    // generic-arg position stays invariant. KNOWN GAP: our current
    // `is_assignable_to` strictly compares TypeIds, which matches the
    // runtime here. (Marked separately so the rule is documented.)
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let i_q = a.nullable(i);
    let arr_i = a.alloc_generic(synth_decl("Array"), vec![i]);
    let arr_iq = a.alloc_generic(synth_decl("Array"), vec![i_q]);
    assert!(!a.is_assignable_to(arr_i, arr_iq));
}

// =============================================================================
// Tuple element-wise (identity on each position)
// =============================================================================

#[test]
fn rt_tuple_identity_allowed() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let s = a.builtin(Builtins::STRING);
    let t1 = a.tuple(synth_decl("Tuple"), i, s);
    let t2 = a.tuple(synth_decl("Tuple"), i, s);
    assert!(a.is_assignable_to(t1, t2));
}

#[test]
fn rt_tuple_element_mismatch_rejected() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let s = a.builtin(Builtins::STRING);
    let f = a.builtin(Builtins::FLOAT);
    let t1 = a.tuple(synth_decl("Tuple"), i, s);
    let t2 = a.tuple(synth_decl("Tuple"), f, s);
    assert!(!a.is_assignable_to(t1, t2));
}

// Asymmetric wildcard direction. The runtime accepts
// `Tuple<int, T>` flowing into `Tuple<any?, any?>` (target raw form
// is the universal sink), but REJECTS the reverse — passing
// `Tuple<any?, any?>{}` to a parameter typed `Tuple<int, T>` raises
// `argument of type 'Tuple' is not assignable to parameter of type
// 'Tuple<int, ...>'`. Probed live with:
//
//   abstract type AbstractType {}
//   fn main() { stats(Tuple<any?, any?> {}); }
//   fn stats(result: Tuple<int, AbstractType>) {}
//
// `greycat run` rejects the call. The bidirectional invariance check
// historically here let P20.1's `Any` source guard masquerade as
// structural equality on each arg, falsely accepting the wrong
// direction.
#[test]
fn rt_tuple_concrete_to_all_any_target_allowed() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let foo = a.alloc_type(synth_decl("Foo"));
    let any_q = a.any_nullable();
    let concrete = a.tuple(synth_decl("Tuple"), i, foo);
    let raw = a.tuple(synth_decl("Tuple"), any_q, any_q);
    assert!(a.is_assignable_to(concrete, raw));
}

#[test]
fn rt_tuple_all_any_source_to_concrete_rejected() {
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let foo = a.alloc_type(synth_decl("Foo"));
    let any_q = a.any_nullable();
    let concrete = a.tuple(synth_decl("Tuple"), i, foo);
    let raw = a.tuple(synth_decl("Tuple"), any_q, any_q);
    assert!(!a.is_assignable_to(raw, concrete));
}

#[test]
fn rt_array_all_any_source_to_concrete_rejected() {
    // Same rule on a single-arg generic — `Array<any?>` does not
    // flow into `Array<int>`. Runtime mirror of the Tuple probe.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let any_q = a.any_nullable();
    let arr_concrete = a.alloc_generic(synth_decl("Array"), vec![i]);
    let arr_raw = a.alloc_generic(synth_decl("Array"), vec![any_q]);
    assert!(a.is_assignable_to(arr_concrete, arr_raw));
    assert!(!a.is_assignable_to(arr_raw, arr_concrete));
}

// =============================================================================
// Cast rules (P12.3) — asymmetric promotions
// =============================================================================

// `rt_cast_int_to_node_tags_allowed` and the inverse
// `rt_cast_node_tags_to_int_allowed` both moved to
// `crate::project::is_castable_with_index` (analysis crate) where
// `WellKnown::is_node_tag(decl)` provides handle-keyed dispatch. The
// pure `is_castable` in this crate no longer knows about node
// tags — coverage lives in the analysis crate's cross-module
// fixtures now.

#[test]
fn rt_cast_string_to_int_rejected() {
    let mut a = arena();
    let s = a.builtin(Builtins::STRING);
    let i = a.builtin(Builtins::INT);
    assert!(!a.is_castable(s, i));
}

#[test]
fn rt_cast_char_to_int_allowed() {
    let mut a = arena();
    let c = a.builtin(Builtins::CHAR);
    let i = a.builtin(Builtins::INT);
    assert!(a.is_castable(c, i));
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
    let symbols = SymbolTable::new();
    let t_sym = symbols.intern("T");
    let u_sym = symbols.intern("U");
    // let owner = GenericOwner::Function(symbols.intern("f"));
    let t1 = a.generic_param(t_sym);
    let t2 = a.generic_param(t_sym);
    let u = a.generic_param(u_sym);
    assert_eq!(t1, t2, "interning should collapse identical GenericParams");
    assert!(a.is_assignable_to(t1, t2));
    assert!(!a.is_assignable_to(t1, u));
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

// `rt_nodetime_t_to_nodetime_nullable_t_allowed`,
// `rt_nodelist_t_to_nodelist_unrelated_t_allowed`,
// `rt_nodeindex_args_are_bivariant` — all three node-tag bivariance
// tests removed. The rule moved to
// `crate::project::is_assignable_to_with_index` (analysis crate),
// dispatching by decl identity via `WellKnown::is_node_tag(decl)`.
// The pure `is_assignable_to` in this crate keeps strict invariance
// on generic args.

#[test]
fn rt_array_int_to_array_nullable_int_still_rejected() {
    // Regression guard: Array is NOT a node tag — the bivariance
    // relaxation must not leak. `Array<int>` ↔ `Array<int?>` remain
    // invariant per the runtime.
    let mut a = arena();
    let i = a.builtin(Builtins::INT);
    let i_q = a.nullable(i);
    let arr_i = a.alloc_generic(synth_decl("Array"), vec![i]);
    let arr_iq = a.alloc_generic(synth_decl("Array"), vec![i_q]);
    assert!(!a.is_assignable_to(arr_i, arr_iq));
    assert!(!a.is_assignable_to(arr_iq, arr_i));
}

// =============================================================================
// Raw-form symmetric assignability — REMOVED
// =============================================================================
//
// The five tests in this section guarded a `Named ↔ Generic` bridge
// rule in `is_assignable_to` that papered over a shape mismatch:
// the body walker and the validation pass produced different
// lowerings for the same raw-form source token (`Tensor` with no
// params became `Named{Tensor}` on one side and stayed `Named` /
// became `Generic{Tensor, args}` on the other). The bridge let
// `Generic{N, [any,any]}` flow into `Named{N}` and vice versa when
// every target arg was `any`.
//
// The fix landed upstream: every type-position lowerer
// (`lower_type_ref`, `lower_type_ref_id`, `lower_type_ref_project`)
// now expands a raw-form generic reference to its canonical
// `Generic{name, [any?; arity]}` form at lowering time. Both passes
// converge on the same shape; the existing `(Generic, Generic)`
// all-any widening rule handles the widening directly. No bridge
// rule is reachable from production code, and these tests asserted
// the bridge's behavior in isolation.

// =============================================================================
// Named{N} ↔ GenericParam{N} name match — REMOVED in P35.6
// =============================================================================
//
// The four tests in this section guarded a workaround in
// `is_assignable_to` that papered over `mint_type_shape` not
// threading generic scope (the validation pass lowered a declared
// `V?` parameter as `Named{name:"V"}` while the body walker
// lowered the same token as `GenericParam{name:"V", owner: ...}`).
// The bridge rule made the two compare equal.
//
// P35.6 removes the bridge along with these tests. No production
// call path lit the arm; the gauntlet tests constructed the pair
// directly to assert the workaround's shape. After 35.7 routes
// foreign type-refs through decl handles, the leak source goes
// away and the workaround becomes unnecessary.
//
// The negative test (`rt_named_v_does_not_match_generic_param_u`)
// also goes: with the bridge rule gone, the answer is trivially
// `false` for every Named ↔ GenericParam pair regardless of name,
// so the assertion was already structural rather than semantic.

// =============================================================================
// is_castable strips source nullable on fallthrough
// =============================================================================
//
// `T? as T` is accepted by the runtime: the cast compiles, and the
// runtime evaluates the actual value's nullity at execution time
// (rejecting only when the value is null). Verified per probe:
//
//   enum PointType { a; b; c; }
//   var v = (Map<String, PointType>{}).get("foo") as PointType;
//
// Build accepts; @test passes when the map entry is non-null.
// `is_castable` must mirror the runtime: the cast itself is well-
// typed even when the source is nullable.

#[test]
fn rt_cast_nullable_decl_to_non_nullable_decl() {
    let mut a = arena();
    let foo = a.alloc_type(synth_decl("Foo"));
    let foo_q = a.nullable(foo);
    assert!(a.is_castable(foo_q, foo));
}

#[test]
fn rt_cast_nullable_enum_shape_to_non_nullable_enum_shape() {
    use greycat_analyzer_core::{Type, TypeKind};
    let mut a = arena();
    let symbols = SymbolTable::new();
    let enum_kind = TypeKind::Enum {
        name: symbols.intern("PointType"),
        variants: vec![
            symbols.intern("a"),
            symbols.intern("b"),
            symbols.intern("c"),
        ]
        .into_boxed_slice(),
    };
    let enum_id = a.alloc(Type {
        kind: enum_kind.clone(),
        nullable: false,
    });
    let enum_q = a.nullable(enum_id);
    assert!(a.is_castable(enum_q, enum_id));
}

#[test]
fn rt_cast_non_nullable_decl_to_self_still_works() {
    // Regression guard: the fallthrough strip kicks in *only* on a
    // nullable source. Non-nullable sources fall through to the
    // standard `is_assignable_to` path; identical decl-keyed shapes
    // must still pass.
    let mut a = arena();
    let foo = a.alloc_type(synth_decl("FooSelf"));
    assert!(a.is_castable(foo, foo));
}
