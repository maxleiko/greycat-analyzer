//! Type system for greycat — foundation port.
//!
//! This crate is the foundation the analyzer builds on; it owns
//! the `Type` enum, type interning, and subtyping rules.
//!
//! What's here:
//! - [`Type`]: the central enum (primitives, decl-keyed types, generics, lambda, etc.)
//! - [`TypeId`]: a `Copy` handle into the [`TypeArena`].
//! - Primitive type ids (`null_t()`, `int_t()`, ...) for cheap comparisons.
//! - [`TypeRegistry`]: holds per-module declared types so lookups
//!   work without walking the HIR every time.
//! - Subtyping (`is_assignable_to`) covering the cases the analyzer needs
//!   in primitive widening, null-into-nullable, generic invariance,
//!   any/never, lambda variance.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::Symbol;
use crate::type_arena::TypeArena;

/// A handle into a [`TypeArena`]. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(pub(crate) u32);

impl TypeId {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for TypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Project-wide unique identifier for a top-level item in a module.
///
/// Composed of `(module, name)` where both halves are [`Symbol`]s
/// interned in the shared `SymbolTable`. Uniqueness across the project
/// is enforced at module-ingest time: any file whose stem collides
/// with an already-ingested module gets a `duplicate-module-name`
/// hard error and is excluded from the project closure.
///
/// Replaces name-only keying on every per-item project map
/// (`type_members`, `fn_signatures`, `enum_types`, `var_types`,
/// `type_flags`, …) so two same-named items in different modules
/// (`foo::Load` and `bar::Load`) coexist unambiguously. Two `ItemKey`s
/// compare equal iff they refer to the same item in the same module —
/// one register-sized compare, since both fields are `Copy` u32
/// newtypes under the hood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemKey {
    pub module: Symbol,
    pub name: Symbol,
}

impl ItemKey {
    pub const fn new(module: Symbol, name: Symbol) -> Self {
        Self { module, name }
    }
}

/// The central type representation.
///
/// The TS reference uses a class hierarchy with `nullable` flags per type
/// instance; we mirror that with a top-level `nullable` field on every
/// variant via the wrapping [`Type`] struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Type {
    pub kind: TypeKind,
    /// `true` iff this type allows `null` as a value (the `T?` syntax).
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeKind {
    /// `null`-only type. Convertible to any nullable.
    Null,
    /// Top type. Anything non-nullable is assignable to it.
    Any,
    /// Bottom type. Used for unreachable code.
    Never,
    /// A resolved non-generic type — user-defined `type Foo {...}` or
    /// a non-generic native type from `std/core` (the 8 primitives
    /// `int float String bool char time duration geo` are exactly the
    /// native-core decls keyed here as `Type(core::X)`). The decl's [`ItemKey`]
    /// `(module, name)` is the identity; cross-module references to
    /// the same decl share the same `ItemKey`, so equality is two
    /// register-sized symbol compares.
    ///
    /// Distinct from [`TypeKind::Generic`] with empty args.
    /// Non-generic types and zero-arg instantiations are different
    /// concepts — separating them by variant lets the substitution /
    /// variance / node-tag-dispatch machinery match only the latter
    /// without runtime `args.is_empty()` checks.
    Type(ItemKey),
    /// An instantiation of a generic template — `Array<int>`, `node<int?>`,
    /// `Map<String, V>`. `tpl` is the generic template's [`ItemKey`];
    /// `args` are the per-use-site type arguments and are guaranteed
    /// non-empty by the lowering pass (zero-arg uses of a generic
    /// template are an analysis error caught upstream).
    Generic {
        tpl: ItemKey,
        /// Cannot be zero-length (ensured by the lowering phase)
        args: SmallVec<[TypeId; 2]>,
    },
    /// A type-ref whose name didn't resolve — typo, missing import,
    /// in-progress code. Carries the source `name` and `byte_range`
    /// (as a `(start, end)` tuple since `Range<usize>` is not `Hash`,
    /// and the arena interns by value) so diagnostics can quote and
    /// locate the offending name verbatim.
    ///
    /// Behaviorally an `any`-like sink: satisfies every assignability
    /// check from both directions so a single unresolved name doesn't
    /// fan out into a cascade of false "incompatible types"
    /// diagnostics; the resolver's "unresolved name" error already
    /// pinpoints the cause.
    ///
    /// Distinct from `Any` so consumers that *want* to know "this was
    /// an unresolved type, render the original name" can — hover and
    /// display show the typo'd name verbatim.
    Unresolved {
        name: Symbol,
        byte_range: (usize, usize),
    },
    /// Generic type *parameter*
    /// - the `T` inside a `fn<T>(x: T)` body
    /// - the `T` inside a `type Box<T> { field: T; }`
    GenericParam(Symbol),
    /// Function / lambda type.
    ///
    /// `ret` is `None` when the source decl / lambda literal did not
    /// declare a return type AND body-inference could not produce a
    /// single GCL-expressible type. `Some(t)` carries the explicit
    /// or inferred return. Display renders `fn(P)` for `None` and
    /// `fn(P): R` for `Some(R)`.
    Lambda {
        /// Can be zero-length
        params: Box<[TypeId]>,
        ret: Option<TypeId>,
    },
    /// Enum type.
    Enum {
        name: Symbol,
        variants: Box<[Symbol]>,
    },
    /// Union of two-or-more alternatives. Construction normalizes:
    /// `T | T = T`, `T | null = nullable(T)`.
    Union { alts: Box<[TypeId]> },
    /// A *type-literal value* — the runtime value is the type `inner`
    /// itself (not an instance of it). Mints from
    /// `typeof T` in source position and from bare type-ident
    /// expressions like `DurationUnit` used in value position. Pairs
    /// with the `typeof T` parameter form so generic inference can
    /// witness `T := inner` when a `typeof T` param meets a
    /// `TypeOf(X)` argument.
    ///
    /// Equality is by inner-`TypeId` only; nullability lives on the
    /// outer [`Type`] wrapper as for every other kind.
    TypeOf(TypeId),
}

/// Where a generic parameter was declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericOwner {
    /// `fn<T>(...)`.
    Function(Symbol),
    /// `type Foo<T> {...}`.
    Type(Symbol),
}

/// Looks up named types.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    /// Maps `Symbol` (an interned type name) → `TypeId` in the shared arena.
    named: FxHashMap<Symbol, TypeId>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn register(&mut self, name: Symbol, id: TypeId) {
        self.named.insert(name, id);
    }

    #[inline]
    pub fn lookup(&self, name: Symbol) -> Option<TypeId> {
        self.named.get(&name).copied()
    }

    /// Iterate every registered name [`Symbol`]. Use the project's
    /// [`SymbolTable`] to recover the source text.
    pub fn iter_names(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.named.keys().copied()
    }
}

/// Per-call constraint table that records "type-parameter `T` was
/// witnessed at type `…`" pairs as the analyzer walks a generic call
/// site. After all arguments have been visited, [`InferenceTable::solve`]
/// substitutes accumulated witnesses into the declared return type.
///
/// **Scope:** records and substitutes simple `GenericParam` ↔ concrete
/// pairs. Variance handling beyond what [`is_assignable_to`] already provides,
/// and union-of-witnesses merging are deferred — this is the seam, not a full Hindley-Milner.
#[derive(Debug, Default)]
pub struct InferenceTable {
    bindings: FxHashMap<Symbol, TypeId>,
}

impl InferenceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a witness for a generic param. If the same param has
    /// already been bound, the new witness is dropped — the analyzer's
    /// caller should already have type-checked it against the prior
    /// witness through [`is_assignable_to`].
    pub fn bind(&mut self, name: Symbol, ty: TypeId) {
        self.bindings.entry(name).or_insert(ty);
    }

    pub fn lookup(&self, name: &Symbol) -> Option<TypeId> {
        self.bindings.get(name).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Substitute every `GenericParam(name)` in `ty` with the recorded
    /// witness. Idempotent — re-applying produces the same result.
    pub fn substitute(&self, arena: &mut TypeArena, ty: TypeId) -> TypeId {
        let t = arena.get(ty).clone();
        match &t.kind {
            TypeKind::GenericParam(name) => {
                if let Some(witness) = self.bindings.get(name) {
                    let nullable = t.nullable;
                    if !nullable {
                        return *witness;
                    }
                    arena.nullable(*witness)
                } else {
                    ty
                }
            }
            TypeKind::Generic { tpl, args } => {
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(arena, *a)).collect();
                if &new_args == args {
                    ty
                } else {
                    arena.alloc(Type {
                        kind: TypeKind::Generic {
                            tpl: *tpl,
                            args: new_args,
                        },
                        nullable: t.nullable,
                    })
                }
            }
            TypeKind::TypeOf(inner) => {
                let new_inner = self.substitute(arena, *inner);
                if new_inner == *inner {
                    ty
                } else {
                    arena.alloc(Type {
                        kind: TypeKind::TypeOf(new_inner),
                        nullable: t.nullable,
                    })
                }
            }
            _ => ty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SymbolTable;

    struct TextCx {
        arena: TypeArena,
        symbols: SymbolTable,
    }

    impl Default for TextCx {
        fn default() -> Self {
            let symbols = SymbolTable::new();
            let arena = TypeArena::new(&symbols);
            Self { arena, symbols }
        }
    }

    impl TextCx {
        /// Mint a synthetic [`ItemKey`] for tests — every test module
        /// shares the same fake module symbol `"test_mod"`, so two
        /// items with the same `name` collapse to the same identity.
        fn item(&self, name: &str) -> ItemKey {
            ItemKey::new(self.symbols.intern("test_mod"), self.symbols.intern(name))
        }
    }

    #[test]
    fn typekind_name_dedups() {
        let mut cx = TextCx::default();
        let array_ty = cx.item("Array");
        let base = cx.arena.len();
        let a = cx
            .arena
            .alloc_generic(array_ty, vec![cx.arena.builtins.int]);
        let b = cx
            .arena
            .alloc_generic(array_ty, vec![cx.arena.builtins.int]);
        assert_eq!(a, b);
        assert_eq!(
            cx.arena.len(),
            base + 1,
            "interning the same generic twice adds one entry"
        );
    }

    #[test]
    fn nullable_idempotent() {
        let mut cx = TextCx::default();
        let base = cx.arena.len();
        let q1 = cx.arena.nullable(cx.arena.builtins.int);
        let q2 = cx.arena.nullable(q1);
        assert_eq!(q1, q2);
        assert_eq!(
            cx.arena.len(),
            base + 1,
            "nullable adds one entry, then is idempotent"
        );
    }

    #[test]
    fn strip_nullable_idempotent() {
        let mut cx = TextCx::default();
        let base = cx.arena.len();
        let ni = cx.arena.nullable(cx.arena.builtins.int);
        let i2 = cx.arena.strip_nullable(ni);
        assert_eq!(cx.arena.builtins.int, i2);
        assert_eq!(
            cx.arena.len(),
            base + 1,
            "only the nullable variant is added; strip reuses the builtin"
        );
    }

    #[test]
    fn primitives_do_not_cross_widen() {
        let cx = TextCx::default();
        let i = cx.arena.builtins.int;
        let f = cx.arena.builtins.float;
        let s = cx.arena.builtins.string;
        let c = cx.arena.builtins.char_;
        assert!(!cx.arena.is_assignable_to(i, f));
        assert!(!cx.arena.is_assignable_to(f, i));
        assert!(!cx.arena.is_assignable_to(c, i));
        assert!(!cx.arena.is_assignable_to(i, c));
        assert!(!cx.arena.is_assignable_to(c, s));
        assert!(!cx.arena.is_assignable_to(s, c));
        assert!(cx.arena.is_assignable_to(i, i));
        assert!(cx.arena.is_assignable_to(f, f));
    }

    #[test]
    fn null_flows_into_nullable_only() {
        let mut cx = TextCx::default();
        let null = cx.arena.null();
        let int = cx.arena.builtins.int;
        let int_q = cx.arena.nullable(int);
        assert!(cx.arena.is_assignable_to(null, int_q));
        assert!(!cx.arena.is_assignable_to(null, int));
    }

    #[test]
    fn nullable_does_not_silently_narrow() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let int_q = cx.arena.nullable(int);
        assert!(cx.arena.is_assignable_to(int, int_q));
        assert!(!cx.arena.is_assignable_to(int_q, int));
    }

    #[test]
    fn any_top_never_bottom() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let any = cx.arena.any();
        let never = cx.arena.never();
        assert!(cx.arena.is_assignable_to(int, any));
        assert!(cx.arena.is_assignable_to(never, int));
    }

    #[test]
    fn generic_invariant_in_args() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let float = cx.arena.builtins.float;
        let array_decl = cx.item("Array");
        let arr_int = cx.arena.alloc_generic(array_decl, vec![int]);
        let arr_float = cx.arena.alloc_generic(array_decl, vec![float]);
        // P12.2 (matches the GreyCat runtime, *not* the TS reference
        // checker): generic args are invariant. Even though `int`
        // widens to `float`, `Array<int>` is **not** assignable to
        // `Array<float>` (the runtime rejects this — we trust the
        // runtime as the oracle). The reverse is also rejected.
        assert!(!cx.arena.is_assignable_to(arr_int, arr_float));
        assert!(!cx.arena.is_assignable_to(arr_float, arr_int));
        assert!(cx.arena.is_assignable_to(arr_int, arr_int));
    }

    #[test]
    fn generic_name_mismatch_stays_unassignable() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let array_decl = cx.item("Array");
        let set_decl = cx.item("Set");
        let arr_int = cx.arena.alloc_generic(array_decl, vec![int]);
        let set_int = cx.arena.alloc_generic(set_decl, vec![int]);
        // Different generic names with the same args still mismatch.
        // Inheritance-aware assignability (`type Child<T> extends
        // Parent<T>`) is a later phase.
        assert!(!cx.arena.is_assignable_to(arr_int, set_int));
    }

    #[test]
    fn lambda_with_any_slot_is_symmetric() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let any = cx.arena.any();
        // After P20.1, `any` is interchangeable with any other type
        // (both top *and* bottom in the lattice — mirrors the runtime
        // which compiles `any → T` and defers the type check). So a
        // lambda with `any` in any slot is mutually assignable with a
        // lambda that has a concrete type in the same slot.
        // `f1: (any) -> int` ↔ `f2: (int) -> any`:
        //   * f1 → f2: param needs `int → any` ✓, return needs `int → any` ✓.
        //   * f2 → f1: param needs `any → int` ✓ (P20.1), return needs `any → int` ✓.
        let f1 = cx.arena.lambda(vec![any], Some(int));
        let f2 = cx.arena.lambda(vec![int], Some(any));
        assert!(cx.arena.is_assignable_to(f1, f2));
        assert!(cx.arena.is_assignable_to(f2, f1));
    }

    #[test]
    fn lambda_arity_mismatch_rejected() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        // Arity mismatch is hard-rejected regardless of the `any`
        // bidirectionality from P20.1 — no slot count, no relation.
        let f1 = cx.arena.lambda(vec![int], Some(int));
        let f2 = cx.arena.lambda(vec![int, int], Some(int));
        assert!(!cx.arena.is_assignable_to(f1, f2));
        assert!(!cx.arena.is_assignable_to(f2, f1));
    }

    #[test]
    fn union_member_flows_in() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let str_t = cx.arena.builtins.string;
        let union = cx.arena.alloc(Type {
            kind: TypeKind::Union {
                alts: Box::new([int, str_t]),
            },
            nullable: false,
        });
        assert!(cx.arena.is_assignable_to(int, union));
        assert!(cx.arena.is_assignable_to(str_t, union));
        let bool_t = cx.arena.builtins.bool_;
        assert!(!cx.arena.is_assignable_to(bool_t, union));
    }

    #[test]
    fn registry_lookup() {
        let mut cx = TextCx::default();
        let mut reg = TypeRegistry::new();
        let foo_decl = cx.item("Foo");
        let foo = cx.arena.alloc_type(foo_decl);
        let foo_sym = cx.symbols.intern("Foo");
        let bar_sym = cx.symbols.intern("Bar");
        reg.register(foo_sym, foo);
        assert_eq!(reg.lookup(foo_sym), Some(foo));
        assert!(reg.lookup(bar_sym).is_none());
    }

    #[test]
    fn symbol_table_intern_is_idempotent() {
        let s = SymbolTable::new();
        let a1 = s.intern("alpha");
        let a2 = s.intern("alpha");
        let b = s.intern("beta");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_eq!(s.resolve(&a1), "alpha");
        assert_eq!(s.resolve(&b), "beta");
        assert_eq!(s.lookup("alpha"), Some(a1));
        assert!(s.lookup("gamma").is_none());
    }

    #[test]
    fn node_tag_does_not_auto_deref_into_inner() {
        // The runtime rejects `var x: T = some_node<T>();` — node
        // dereferencing is a *syntactic* deref (`*n` / `n->m()`
        // desugars to `n.resolve().m()` via the `@deref("resolve")`
        // annotation on `node<T>`'s decl), not an assignability rule.
        // Earlier ports of this analyzer baked an `is_node_tag(name) &&
        // args[0] assigns to to` rule into `is_assignable_to`; the
        // runtime oracle disagreed.
        let mut cx = TextCx::default();
        let person_decl = cx.item("Person");
        let person = cx.arena.alloc_type(person_decl);
        let node_decl = cx.item("node");
        let node_person = cx.arena.alloc_generic(node_decl, vec![person]);
        assert!(!cx.arena.is_assignable_to(node_person, person));
        assert!(!cx.arena.is_assignable_to(person, node_person));
    }

    #[test]
    fn inference_table_substitutes_generic_params() {
        let mut cx = TextCx::default();
        let t_sym = cx.symbols.intern("T");
        // let foo_sym = cx.symbols.intern("Foo");
        let int = cx.arena.builtins.int;
        let t_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam(t_sym),
            nullable: false,
        });
        let array_decl = cx.item("Array");
        let arr_t = cx.arena.alloc_generic(array_decl, vec![t_param]);

        let mut tbl = InferenceTable::new();
        tbl.bind(t_sym, int);

        let resolved = tbl.substitute(&mut cx.arena, arr_t);
        let resolved_kind = &cx.arena.get(resolved).kind;
        let TypeKind::Generic { tpl, args } = resolved_kind else {
            panic!("expected Array<int>, got {resolved_kind:?}");
        };
        assert_eq!(*tpl, array_decl);
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int]);
    }

    #[test]
    fn arena_substitute_replaces_generic_params() {
        let mut cx = TextCx::default();
        let t_sym = cx.symbols.intern("T");
        let u_sym = cx.symbols.intern("U");
        // let foo_sym = cx.symbols.intern("Foo");
        let int = cx.arena.builtins.int;
        let str_t = cx.arena.builtins.string;
        let t_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam(t_sym),
            nullable: false,
        });
        let u_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam(u_sym),
            nullable: false,
        });
        let array_decl = cx.item("Array");
        let map_decl = cx.item("Map");
        let map_tu = cx.arena.alloc_generic(map_decl, vec![t_param, u_param]);

        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        subst.insert(t_sym, int);
        subst.insert(u_sym, str_t);

        let resolved = cx.arena.substitute(map_tu, &subst);
        let TypeKind::Generic { tpl, args } = &cx.arena.get(resolved).kind else {
            panic!("expected Map<int, String>");
        };
        assert_eq!(*tpl, map_decl);
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int, str_t]);

        // Idempotent: re-applying yields the same TypeId.
        let resolved2 = cx.arena.substitute(resolved, &subst);
        assert_eq!(resolved, resolved2);

        // Nullability preserved: Array<T?> with T → int gives Array<int?>.
        let t_param_q = cx.arena.nullable(t_param);
        let arr_t_q = cx.arena.alloc_generic(array_decl, vec![t_param_q]);
        let resolved_q = cx.arena.substitute(arr_t_q, &subst);
        let TypeKind::Generic { args: q_args, .. } = &cx.arena.get(resolved_q).kind else {
            panic!();
        };
        assert!(cx.arena.get(q_args[0]).nullable);
    }

    #[test]
    fn arena_substitute_no_op_on_empty_subst() {
        let mut cx = TextCx::default();
        let int = cx.arena.builtins.int;
        let array_decl = cx.item("Array");
        let arr = cx.arena.alloc_generic(array_decl, vec![int]);
        let empty: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        assert_eq!(cx.arena.substitute(arr, &empty), arr);
    }
}
