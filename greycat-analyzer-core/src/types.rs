//! Type system for greycat — foundation port.
//!
//! Ports the core of `packages/lang/src/analysis/types.ts` (~2,811 LoC of
//! TS). This crate is the foundation the analyzer builds on; it owns
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
//!
//! What's deferred:
//! - Full TS subtyping rules around node types and runtime tagging.
//! - Variance for user-declared generics (TS treats them invariantly).
//! - Inference table / unification beyond simple substitution.
//!
//! Decision B: single typed AST + type arena (no separate hir-def/hir-ty
//! split). Inference table is a thin map from `Idx<Expr>` to `TypeId`
//! and lives in the analyzer crate, not here.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{Symbol, SymbolTable};

/// A handle into a [`TypeArena`]. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl TypeId {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
}

// P35.1
/// Stable, project-wide handle to a resolved type-decl
/// (`Decl::Type` or `Decl::Enum`). Dense `u32` newtype, `Copy`,
/// hashable in one register-sized compare. Issued by the project's
/// `DeclRegistry`; resolves back to its `(Uri, Idx<Decl>)` source
/// through that registry.
///
/// Two `TypeDeclId`s compare equal iff they point at the same decl in
/// the same module. A user-declared `type node<T>` and the std-core
/// `node<T>` therefore get different handles — the soundness gap where
/// the previous SmolStr-keyed identity collapsed them is closed at
/// the type-system level.
///
/// Not comparable across distinct `DeclRegistry` instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeDeclId(u32);

impl TypeDeclId {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u32 {
        self.0
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
    /// `any` — top type. Anything non-nullable is assignable to it.
    Any,
    /// `never` — bottom type. Used for unreachable code.
    Never,
    /// Named primitive — `int`, `float`, `String`, `bool`, `char`,
    /// `time`, `duration`, `geo`. Carries the canonical name.
    Primitive(Primitive),
    // P35.2
    /// A resolved non-generic type — user-defined `type Foo {...}` or
    /// a non-generic native type from `std/core`. The decl handle is
    /// the identity; cross-module references to the same decl share
    /// the same `TypeDeclId`, so equality is one register-sized
    /// compare.
    ///
    /// Distinct from [`TypeKind::Generic`] with empty args.
    /// Non-generic types and zero-arg instantiations are different
    /// concepts — separating them by variant lets the substitution /
    /// variance / node-tag-dispatch machinery match only the latter
    /// without runtime `args.is_empty()` checks.
    Type(TypeDeclId),
    // P35.2
    /// An instantiation of a generic decl — `Array<int>`, `node<int?>`,
    /// `Map<String, V>`. `decl` is the generic template's handle;
    /// `args` are the per-use-site type arguments and are guaranteed
    /// non-empty by the lowering pass (zero-arg uses of a generic
    /// decl are an analysis error caught upstream).
    Generic {
        decl: TypeDeclId,
        /// Cannot be zero-length (ensured by the lowering phase)
        args: SmallVec<[TypeId; 2]>,
    },
    // P35.3
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
    /// Generic type *parameter* — the `T` inside a `fn<T>(x: T)` body.
    // P25.4
    GenericParam { name: Symbol, owner: GenericOwner },
    /// Function / lambda type.
    Lambda {
        /// Can be zero-length
        params: Box<[TypeId]>,
        /// TODO: should this be `Option<TypeId>` because return-type is optional in GCL
        ret: TypeId,
    },
    /// Enum type.
    // P25.4
    Enum {
        name: Symbol,
        variants: Box<[Symbol]>,
    },
    /// Union of two-or-more alternatives. Construction normalizes:
    /// `T | T = T`, `T | null = nullable(T)`.
    Union { alts: Vec<TypeId> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    Bool,
    Int,
    Float,
    Char,
    String,
    Time,
    Duration,
    Geo,
}

impl Primitive {
    pub fn name(self) -> &'static str {
        match self {
            Primitive::Bool => "bool",
            Primitive::Int => "int",
            Primitive::Float => "float",
            Primitive::Char => "char",
            Primitive::String => "String",
            Primitive::Time => "time",
            Primitive::Duration => "duration",
            Primitive::Geo => "geo",
        }
    }
}

/// Where a generic parameter was declared.
// P25.4
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericOwner {
    /// `fn<T>(...)`.
    Function(Symbol),
    /// `type Foo<T> {...}`.
    Type(Symbol),
}

// =============================================================================
// Arena
// =============================================================================

/// Append-only interning arena for `Type`. Two equal `Type` values get
/// the same [`TypeId`]; comparing for equality is then just an integer
/// comparison.
///
/// The arena also owns a parallel `decl_names` table indexed by
/// `TypeDeclId.raw()`. Every `alloc_type` / `generic` records the
/// decl's source name there, so `arena.display(id)` can
/// recover a printable name without a callback or a borrow on the
/// project's `DeclRegistry`. The duplication is a deliberate
/// denormalisation: `DeclRegistry` stays the canonical `(uri, decl) →
/// handle` interner; `decl_names` is a read-path view scoped to one
/// arena.
#[derive(Debug, Default, Clone)]
pub struct TypeArena {
    pub items: Vec<Type>,
    pub intern: FxHashMap<Type, TypeId>,
    // decl_names: Vec<SmolStr>,
}

impl TypeArena {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&mut self, ty: Type) -> TypeId {
        if let Some(&id) = self.intern.get(&ty) {
            return id;
        }
        let id = TypeId(self.items.len() as u32);
        self.items.push(ty.clone());
        self.intern.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &Type {
        &self.items[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Make a copy of `id` with `nullable = true`. Idempotent.
    pub fn nullable(&mut self, id: TypeId) -> TypeId {
        let mut ty = self.get(id).clone();
        if ty.nullable {
            return id;
        }
        ty.nullable = true;
        self.alloc(ty)
    }

    pub fn primitive(&mut self, p: Primitive) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Primitive(p),
            nullable: false,
        })
    }

    pub fn null(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Null,
            nullable: true,
        })
    }

    /// Strict (non-nullable) `any`. Top of all *non-null* values.
    /// `null → any` is a type error under GreyCat's strict
    /// null-checking — only `any_nullable` accepts null.
    ///
    /// Most callers in the analyzer want
    /// [`Self::any_nullable`]; reach for this only when the
    /// surface syntax was `any` *without* a `?` and you need to
    /// preserve that non-null guarantee through the type system.
    pub fn any(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Any,
            nullable: false,
        })
    }

    /// Nullable `any` — top of *all* values including null.
    /// `null → any_nullable` is allowed. Equivalent to writing
    /// `any?` in source. Used as the fallback for unresolved
    /// names, for generic raw-form arg slots (`Tensor` ≡
    /// `Tensor<any?, any?>`), and any other place where a
    /// universally-permissive type is the right answer.
    pub fn any_nullable(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Any,
            nullable: true,
        })
    }

    pub fn never(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Never,
            nullable: false,
        })
    }

    // P35.2
    /// Allocate a resolved non-generic [`TypeKind::Type`]. Registers
    /// `name` with the arena so [`Self::display`] can render the type
    /// without a callback or a `DeclRegistry` borrow. Name registration
    /// is idempotent on `decl`.
    pub fn alloc_type(
        &mut self,
        decl: TypeDeclId,
        // name: impl Into<SmolStr>,
    ) -> TypeId {
        // self.register_decl_name(decl, name.into());
        self.alloc(Type {
            kind: TypeKind::Type(decl),
            nullable: false,
        })
    }

    // P35.2
    /// Allocate a [`TypeKind::Generic`] (decl-keyed generic
    /// instantiation). Caller guarantees `args` is non-empty —
    /// zero-arg uses of a generic decl are an upstream lowering
    /// error, not a value-shaped concept. `name` is registered with
    /// the arena (same idempotency as [`Self::alloc_type`]).
    pub fn generic(
        &mut self,
        decl: TypeDeclId,
        // name: impl Into<SmolStr>,
        args: Vec<TypeId>,
    ) -> TypeId {
        debug_assert!(!args.is_empty(), "Generic must have non-empty args");
        // self.register_decl_name(decl, name.into());
        self.alloc(Type {
            kind: TypeKind::Generic {
                decl,
                args: args.into(),
            },
            nullable: false,
        })
    }

    // Idempotently record `name` as the printable source name for
    // `decl`. First call wins; subsequent calls with a matching name
    // no-op, conflicting names trip a debug-assert (the decl identity
    // is supposed to be 1:1 with its name).
    // fn register_decl_name(&mut self, decl: TypeDeclId, name: SmolStr) {
    //     let i = decl.raw() as usize;
    //     if i >= self.decl_names.len() {
    //         self.decl_names.resize(i + 1, SmolStr::default());
    //     }
    //     if self.decl_names[i].is_empty() {
    //         self.decl_names[i] = name;
    //     } else {
    //         debug_assert_eq!(
    //             self.decl_names[i],
    //             name,
    //             "TypeDeclId {} re-registered with a different name",
    //             decl.raw()
    //         );
    //     }
    // }

    // Decl-name resolution lives one layer up — see
    // `ProjectAnalysis::decl_name` in the analysis crate, which goes
    // through `DeclRegistry::name(id) -> Symbol` then resolves the
    // symbol against the project's [`SymbolTable`]. `TypeArena`
    // intentionally does not store names; it only knows
    // [`TypeDeclId`] handles.

    /// Return a [`Display`]-implementing wrapper that renders the type
    /// at `id`. Reads decl names from this arena's `decl_names` table —
    /// no callback, no registry borrow.
    pub fn display<'s>(&self, id: TypeId, symbols: &'s SymbolTable) -> TypeDisplay<'_, 's> {
        TypeDisplay {
            arena: self,
            symbols,
            id,
        }
    }

    // P35.3
    /// Allocate a [`TypeKind::Unresolved`]. Use this in place of the
    /// `arena.any()` fallback when a type-ref name didn't resolve —
    /// behaves like `any` for assignability but carries the source
    /// name + span for diagnostic rendering. Nullable to match
    /// `any`'s semantics: an unresolved name has no constraint
    /// against null.
    pub fn unresolved(&mut self, name: Symbol, byte_range: (usize, usize)) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Unresolved { name, byte_range },
            nullable: true,
        })
    }

    pub fn generic_param(&mut self, name: Symbol, owner: GenericOwner) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::GenericParam { name, owner },
            nullable: false,
        })
    }

    pub fn lambda(&mut self, params: Vec<TypeId>, ret: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Lambda {
                params: params.into_boxed_slice(),
                ret,
            },
            nullable: false,
        })
    }

    /// `(x, y)` tuple-literal type, modelled as `Tuple<X, Y>` per
    /// the compiler's desugaring rule (mirrors `[42]` ≡
    /// `Array<int>{42}`). Strictly 2-element — the grammar's
    /// `tuple_expr` rule emits exactly `(left, right)` and nothing
    /// else, so the type is always a pair. `decl` is the std-core
    /// `Tuple` decl handle the caller has pulled from
    /// `WellKnown::tuple_decl`.
    pub fn tuple(&mut self, decl: TypeDeclId, x: TypeId, y: TypeId) -> TypeId {
        self.generic(decl, vec![x, y])
    }

    // P19
    /// Substitute `GenericParam(name)` occurrences inside `ty`
    /// with the matching entry in `subst`, allocating fresh interned
    /// types for any container that changed shape. Idempotent: calling
    /// twice produces the same TypeId. Mirrors
    /// [`InferenceTable::substitute`] but takes a plain `&FxHashMap` so
    /// callers (e.g. the staged-pipeline body walker) don't have to
    /// route witnesses through an `InferenceTable`.
    ///
    /// Recurses through `Generic`, `Tuple`, `Lambda`, `Anonymous`, and
    /// `Union` shapes. Non-substitutable kinds (`Type`, `Primitive`,
    /// `Null`, `Any`, `Never`, `Enum`, `Unresolved`) return `ty`
    /// unchanged.
    pub fn substitute(&mut self, ty: TypeId, subst: &FxHashMap<Symbol, TypeId>) -> TypeId {
        if subst.is_empty() {
            return ty;
        }
        let t = self.get(ty).clone();
        match &t.kind {
            TypeKind::GenericParam { name, .. } => match subst.get(name) {
                Some(&witness) if t.nullable => self.nullable(witness),
                Some(&witness) => witness,
                None => ty,
            },
            // P35.2 — `Type(decl)` is non-generic, no params to substitute.
            TypeKind::Type(_) => ty,
            TypeKind::Generic { decl, args } => {
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(*a, subst)).collect();
                if new_args == *args {
                    ty
                } else {
                    let decl = *decl;
                    // Re-use the name already registered for `decl` when
                    // we minted the original `Generic` — no caller-side
                    // bookkeeping needed.
                    // let name = SmolStr::from(
                    //     self.decl_name(decl)
                    //         .expect("decl name registered at first alloc"),
                    // );
                    let mut new_t = self.generic(decl, new_args.into_vec());
                    if t.nullable {
                        new_t = self.nullable(new_t);
                    }
                    new_t
                }
            }
            TypeKind::Lambda { params, ret } => {
                let new_params: Vec<TypeId> =
                    params.iter().map(|p| self.substitute(*p, subst)).collect();
                let new_ret = self.substitute(*ret, subst);
                if new_ret == *ret && new_params.as_slice() == params.as_ref() {
                    ty
                } else {
                    let mut new_t = self.lambda(new_params, new_ret);
                    if t.nullable {
                        new_t = self.nullable(new_t);
                    }
                    new_t
                }
            }
            TypeKind::Union { alts } => {
                let new_alts: Vec<TypeId> =
                    alts.iter().map(|a| self.substitute(*a, subst)).collect();
                if new_alts == *alts {
                    ty
                } else {
                    let mut new_t = self.alloc(Type {
                        kind: TypeKind::Union { alts: new_alts },
                        nullable: false,
                    });
                    if t.nullable {
                        new_t = self.nullable(new_t);
                    }
                    new_t
                }
            }
            _ => ty,
        }
    }
}

// =============================================================================
// Type registry — holds module-level declared types
// =============================================================================

/// Looks up named types.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    /// Maps `Symbol` (an interned type name) → `TypeId` in the
    /// shared arena.
    // P25.2
    named: FxHashMap<Symbol, TypeId>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: Symbol, id: TypeId) {
        self.named.insert(name, id);
    }

    pub fn lookup(&self, name: Symbol) -> Option<TypeId> {
        self.named.get(&name).copied()
    }

    // P19.6
    /// Iterate every registered name [`Symbol`]. Use the project's
    /// [`SymbolTable`] to recover the source text.
    pub fn iter_names(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.named.keys().copied()
    }
}

// =============================================================================
// Subtyping
// =============================================================================

/// `true` iff a value of `from` is assignable to a slot expecting `to`.
/// The relation handles primitive widening (int → float), nullability
/// (T → T?), top/bottom (anything → any, never → anything), and shape
/// matches for generics / tuples / lambdas. User-declared generics are
/// invariant in their parameters (TS reference behavior).
///
/// Returns `false` for shapes the relation hasn't been formally taught
/// — better to under-accept and surface false negatives in  than to
/// silently widen.
pub fn is_assignable_to(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    if from == to {
        return true;
    }
    let a = arena.get(from);
    let b = arena.get(to);

    // Null handling: `null` flows into anything nullable.
    if matches!(a.kind, TypeKind::Null) {
        return b.nullable;
    }
    // `never` flows everywhere.
    if matches!(a.kind, TypeKind::Never) {
        return true;
    }
    // `any` is the top type — everything flows into it.
    if matches!(b.kind, TypeKind::Any) {
        return true;
    }
    // **P20.1** — `any` is *also* the bottom type. The GreyCat
    // compiler accepts `any → T` for any `T` (it compiles cleanly
    // and defers the type check to runtime assignment / call time);
    // the static analyzer must match. This mirrors TypeScript's
    // `any` semantics where the type is both top and bottom. Source
    // nullability is ignored: `any?` → `T` also passes (the runtime
    // compiles it; null at runtime would fail the same way a wrong
    // type would).
    if matches!(a.kind, TypeKind::Any) {
        return true;
    }
    // P35.3 — `Unresolved` behaves like `any` (both top and bottom)
    // so a single unresolved name doesn't cascade into a swarm of
    // false-positive assignability diagnostics. The resolver's
    // "unresolved name" error already pinpoints the cause; we
    // suppress downstream type-relation noise.
    if matches!(a.kind, TypeKind::Unresolved { .. })
        || matches!(b.kind, TypeKind::Unresolved { .. })
    {
        return true;
    }
    // A non-nullable can't widen into a different non-nullable type just
    // because of nullability difference: `T → T?` is fine, `T? → T` is not.
    if a.nullable && !b.nullable {
        return false;
    }

    // P7.3 (REMOVED): there is no `node<T> → T` auto-deref subtype
    // rule. The runtime rejects `var x: T = some_node<T>();` — the
    // arrow operator (`*n` / `n->m()`) is the *syntactic* desugar for
    // `n.resolve().m()`, dispatched by the `@deref("resolve")`
    // annotation on the receiver's type decl, not by an assignability
    // rule baked into the type system. The check survives for the
    // arrow / star handlers in the analyzer (where it reads
    // `TypeFlags::deref` to decide what method to call), not here.

    match (&a.kind, &b.kind) {
        (TypeKind::Primitive(pa), TypeKind::Primitive(pb)) => primitive_assignable(*pa, *pb),
        (
            TypeKind::Lambda {
                params: aparams,
                ret: aret,
            },
            TypeKind::Lambda {
                params: bparams,
                ret: bret,
            },
        ) => {
            // Contravariant in params, covariant in return. Same as TS.
            aparams.len() == bparams.len()
                && aparams
                    .iter()
                    .zip(bparams.as_ref())
                    .all(|(p_a, p_b)| is_assignable_to(arena, *p_b, *p_a))
                && is_assignable_to(arena, *aret, *bret)
        }
        (TypeKind::Union { alts }, _) => {
            // Union assigns into `to` iff every alt does.
            alts.iter().all(|a| is_assignable_to(arena, *a, to))
        }
        (_, TypeKind::Union { alts }) => {
            // Single value flows into a union if it matches *any* alt.
            alts.iter().any(|b| is_assignable_to(arena, from, *b))
        }
        (TypeKind::Enum { name: na, .. }, TypeKind::Enum { name: nb, .. }) => na == nb,
        // P35.2 — decl-handle identity. `Type(decl)` and
        // `Generic { decl, .. }` compare by handle equality;
        // generic args follow runtime-oracle invariance with an
        // "all-any wildcard" escape.
        //
        // P12.2: invariant in every generic parameter. The TS
        // reference checker (`GreycatGenericType.isAssignableTo`)
        // implements covariance, but the GreyCat runtime — the
        // true oracle — rejects covariant assignment (e.g.
        // `Array<float>` is *not* assignable to `Array<int>`).
        // We follow the runtime, not the TS checker. Supertype-
        // chain assignability across different generic names
        // (`type Child<T> extends Parent<T>`) is a later phase.
        //
        // **P19.10** — invariance is checked by *bidirectional*
        // `is_assignable_to` rather than raw `TypeId ==`, so cross-
        // arena lowering variations that produce different but
        // mutually-assignable arg shapes still match.
        //
        // **P19.14** — when *every* target arg is `any`
        // (`Foo<X,Y>` → `Foo<any,any>`), the target acts as a
        // raw-form wildcard and accepts. This matches the
        // runtime, which accepts `Array<int>` → `Array<any>`,
        // `Map<S,int>` → `Map<any,any>`, etc.
        //
        // Per-arg `any` widening (e.g. `Map<S,int>` →
        // `Map<S,any>`) is NOT generally accepted by the
        // runtime — for user-defined `Pair<A,B>` and the
        // V-slot of `Map<K,V>` the runtime rejects partial
        // wildcards. We follow the runtime conservatively
        // and only accept when *all* target args are `any`.
        // Otherwise, args are invariant (P12.2).
        //
        // Node-tag bivariance (`nodeTime<X> ↔ nodeTime<X?>` etc.)
        // lives in `is_assignable_to_with_index` (analysis crate)
        // where `WellKnown::is_node_tag(decl)` provides handle-
        // keyed dispatch. The pure types crate keeps only
        // invariant arg comparison.
        (TypeKind::Type(da), TypeKind::Type(db)) => da == db,
        (TypeKind::Generic { decl: da, args: aa }, TypeKind::Generic { decl: db, args: ab }) => {
            if da == db
                && aa.len() == ab.len()
                && !ab.is_empty()
                && ab
                    .iter()
                    .all(|y| matches!(arena.get(*y).kind, TypeKind::Any))
            {
                return true;
            }
            da == db
                && aa.len() == ab.len()
                && aa.iter().zip(ab).all(|(x, y)| {
                    if *x == *y {
                        return true;
                    }
                    is_assignable_to(arena, *x, *y) && is_assignable_to(arena, *y, *x)
                })
        }
        _ => false,
    }
}

// `is_node_tag(name: &str)` removed. Name-keyed dispatch let a
// user-declared `type node<T> {}` impersonate the std-core tag and
// pick up bivariance / cast semantics it shouldn't. The handle-keyed
// `WellKnown::is_node_tag(decl)` in
// [`greycat_analyzer_analysis::well_known`] is the only correct
// dispatch — node-tag-specific rules now live in
// `is_assignable_to_with_index` / `is_castable_with_index` in the
// analysis crate, where `WellKnown` is reachable.

// =============================================================================
// Inference table (P7.4 — foundational pass)
// =============================================================================

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
            TypeKind::GenericParam { name, .. } => {
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
            TypeKind::Generic { decl, args } => {
                // P25.7
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(arena, *a)).collect();
                if new_args == *args {
                    ty
                } else {
                    let decl = *decl;
                    // Re-use the name already registered for `decl` when
                    // we minted the original `Generic`.
                    // let name = SmolStr::from(
                    //     arena
                    //         .decl_name(decl)
                    //         .expect("decl name registered at first alloc"),
                    // );
                    let mut new_t = arena.generic(decl, new_args.into_vec());
                    if t.nullable {
                        new_t = arena.nullable(new_t);
                    }
                    new_t
                }
            }
            _ => ty,
        }
    }
}

/// `true` iff `from` can be casted to `to` via the GreyCat `as` operator.
///
/// Mirrors the TS reference's `isCastable` (`packages/lang/src/analysis/
/// utils.ts:360`). Cast rules are asymmetric to assignability — `int as
/// nodeTime` is allowed even though `int` doesn't assign-flow into
/// `nodeTime`. Implements (deeper node-tag rules):
/// - `any → any` always.
/// - Nullables: `T?` casts the same as `T`.
/// - `int ↔ {int, float, node{,Time,List,Index,Geo}}`.
/// - `float ↔ {int, float}`.
/// - `node{,Time,List,Index,Geo} ↔ {self, int}`.
/// - `String ↔ String`.
/// - `char ↔ {char, String, int}`.
/// - `bool ↔ bool`.
/// - Enums → `int`.
/// - Anything else falls through to "same head name OR `from` assignable
///   to `to` (no inheritance check yet — that lands when supertype
///   chains thread through the analyzer)".
pub fn is_castable(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    // trivial cast to itself is valid
    if from == to {
        return true;
    }

    let from_t = arena.get(from);
    let to_t = arena.get(to);

    // any target absorbs any non-null source.
    if matches!(to_t.kind, TypeKind::Any) && !from_t.nullable {
        return true;
    }
    // **P19.14** — casting *to* a generic-param target also passes;
    // runtime checks at instantiation time.
    if matches!(to_t.kind, TypeKind::GenericParam { .. }) {
        return true;
    }
    // `Unresolved` (and `Any` as a source) propagate the "behaves
    // like any" rule from `is_assignable_to` — the resolver has
    // already flagged the unresolved name, downstream cast checks
    // shouldn't cascade.
    if matches!(to_t.kind, TypeKind::Unresolved { .. } | TypeKind::Any)
        || matches!(from_t.kind, TypeKind::Unresolved { .. } | TypeKind::Any)
    {
        return true;
    }

    // Union: cast iff all alt casts.
    if let TypeKind::Union { alts } = &from_t.kind {
        return alts.iter().all(|a| is_castable(arena, *a, to));
    }

    // Casting an enum to an `int` is valid
    if matches!(from_t.kind, TypeKind::Enum { .. }) && is_int_target(to_t) {
        return true;
    }

    match &from_t.kind {
        TypeKind::Any => true,
        // **P19.14** — `T as Foo` (where `T` is a generic param)
        // is allowed: the runtime decides at instantiation time.
        // Same for the symmetric `Foo as T` direction.
        TypeKind::GenericParam { .. } => true,
        // `int as <node-tag>` (and the inverse) requires
        // handle-keyed dispatch via `WellKnown::is_node_tag`; lives in
        // the analysis crate's `is_castable_with_index`. Core only
        // knows `int as float` here — every other extending rule
        // moved one layer up.
        TypeKind::Primitive(Primitive::Int) => {
            matches!(to_t.kind, TypeKind::Primitive(Primitive::Float))
        }
        TypeKind::Primitive(Primitive::Float) => {
            matches!(to_t.kind, TypeKind::Primitive(Primitive::Int))
        }
        TypeKind::Primitive(Primitive::Char) => matches!(
            to_t.kind,
            TypeKind::Primitive(Primitive::String) | TypeKind::Primitive(Primitive::Int)
        ),
        // Node-tag heads casting to int (the underlying 64-bit
        // handle representation) moved to `is_castable_with_index`
        // in the analysis crate — handle-keyed dispatch via
        // `WellKnown::is_node_tag(decl)`. Same-head identity is
        // covered by the `is_assignable_to_strip_source_nullable`
        // fallthrough below.
        _ => is_assignable_to_strip_source_nullable(arena, from, to),
    }
}

/// Same as `is_assignable_to` but treats the source as if its nullable
/// flag were stripped. Used by `is_castable`'s fallthrough: a cast is
/// permitted to coerce `T?` to a non-nullable target — the runtime
/// decides at execution time whether the actual value can land there.
fn is_assignable_to_strip_source_nullable(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    let from_t = arena.get(from);
    if !from_t.nullable {
        return is_assignable_to(arena, from, to);
    }
    // Re-implement the cheap kind-based dispatch from `is_assignable_to`
    // but skip the `a.nullable && !b.nullable` early-bail. The interesting
    // shapes for cast-fallthrough are decl-handle / Enum identity and
    // primitive widening — broader generic / lambda compatibility is
    // rare in the `as` position but we delegate to `is_assignable_to`
    // after the shape match.
    let to_t = arena.get(to);
    if matches!(from_t.kind, TypeKind::Null) {
        return to_t.nullable;
    }
    if matches!(from_t.kind, TypeKind::Never) {
        return true;
    }
    if matches!(to_t.kind, TypeKind::Any) {
        return true;
    }
    match (&from_t.kind, &to_t.kind) {
        (TypeKind::Type(da), TypeKind::Type(db)) if da == db => true,
        (TypeKind::Enum { name: na, .. }, TypeKind::Enum { name: nb, .. }) if na == nb => true,
        (TypeKind::Primitive(pa), TypeKind::Primitive(pb)) => primitive_assignable(*pa, *pb),
        _ => false,
    }
}

fn is_int_target(t: &Type) -> bool {
    matches!(t.kind, TypeKind::Primitive(Primitive::Int))
}

fn primitive_assignable(from: Primitive, to: Primitive) -> bool {
    // P12.4: GreyCat's runtime rejects every primitive-to-primitive
    // widening at parameter / variable binding (verified via
    // `greycat run`: `var i: int = 1; take(i)` against `take(_: float)`
    // is rejected as "argument of type 'int' is not assignable to
    // parameter '_' of type 'float'"). Literals can lower to a
    // matching primitive at use site (`var f: float = 1` is fine
    // because `1` lowers to `float` in that position) but bindings
    // do not widen. Even `int → float`, the canonical TS-reference
    // widening, fails. Mirror the runtime: identity only.
    from == to
}

// =============================================================================
// Display
// =============================================================================

/// `Display`-implementing wrapper returned by [`TypeArena::display`].
///
/// Renders a [`TypeId`] using the arena's `decl_names` table to recover
/// printable names for [`TypeKind::Type`] / [`TypeKind::Generic`].
/// When a decl handle hasn't been registered yet (an internal invariant
/// violation, since every alloc registers a name), falls back to the
/// `?type#<raw>` placeholder so the output stays distinguishable.
///
/// Writes straight into the formatter — no intermediate `String`.
pub struct TypeDisplay<'a, 's> {
    arena: &'a TypeArena,
    symbols: &'s SymbolTable,
    id: TypeId,
}

impl std::fmt::Display for TypeDisplay<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type(f, self.arena, self.symbols, self.id)
    }
}

fn write_type(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    symbols: &SymbolTable,
    id: TypeId,
) -> std::fmt::Result {
    let ty = arena.get(id);
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Primitive(p) => f.write_str(p.name())?,
        // Decl-name rendering belongs to the project layer
        // (see `ProjectTypeDisplay` in
        // `greycat-analyzer-analysis::project`). Core's bare display
        // doesn't have access to a `DeclRegistry → Symbol` map, so it
        // renders named types as the `?type#<raw>` placeholder.
        TypeKind::Type(d) => write!(f, "?type#{}", d.raw())?,
        TypeKind::Generic { decl, args } => {
            write!(f, "?type#{}", decl.raw())?;
            write_args(f, arena, symbols, args)?;
        }
        TypeKind::Unresolved { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::GenericParam { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type(f, arena, symbols, *p)?;
            }
            f.write_str(") -> ")?;
            write_type(f, arena, symbols, *ret)?;
        }
        TypeKind::Enum { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::Union { alts } => {
            // Unions render as `A | B | …`. When the union is also
            // nullable and no `Null` alt is already present, append
            // an explicit `| null` — the `?` suffix would visually
            // bind to the last alt only and read wrong. Mirrors the
            // TS reference: simple types get `T?`, narrowing-introduced
            // unions get `T | U | null`.
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type(f, arena, symbols, *a)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
    }
    // `?` suffix for nullable types — except `Null` (redundant) and
    // `Union` (handled inline above with an explicit `| null` alt
    // so the suffix doesn't visually bind to the last alt only).
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

fn write_args(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    symbols: &SymbolTable,
    args: &[TypeId],
) -> std::fmt::Result {
    f.write_str("<")?;
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_type(f, arena, symbols, *a)?;
    }
    f.write_str(">")
}

// P18.1
/// Fully-qualified-name display, matching the GreyCat canonical
/// printer (e.g. `core::int`, `core::Array<core::int?>`,
/// `project::Foo`).
///
/// `home_lib` resolves a Type/Generic/Enum's home module (e.g. `Foo →
/// "project"`, `node → "core"`). Returning `None` falls back to the
/// `core` library — matches the TS reference's behavior for builtins
/// not in the project decl table.
///
/// Differences from [`display`]:
/// - Primitives, builtin runtime types, and unresolved names get a
///   `core::` prefix.
/// - User types resolve to `<lib>::<Name>` via `home_lib`.
///
/// Nullability is rendered with the `?` suffix (same as `display`)
/// for every kind except `Null`. Strict `any` renders as `core::any`,
/// nullable `any?` renders as `core::any?`.
pub fn display_fqn(
    arena: &TypeArena,
    symbols: &SymbolTable,
    id: TypeId,
    home_lib: &dyn Fn(&str) -> Option<String>,
) -> String {
    let ty = arena.get(id);
    let mut s = match &ty.kind {
        // TS reference's `dump-types` emits the bare null literal as
        // `null`, not `core::null` — match that.
        TypeKind::Null => "null".to_string(),
        TypeKind::Any => "core::any".to_string(),
        TypeKind::Never => "core::never".to_string(),
        TypeKind::Primitive(p) => format!("core::{}", p.name()),
        // Decl-name rendering belongs to the project layer.
        // `display_fqn` doesn't have access to a `TypeDeclId →
        // Symbol` map (that lives on `DeclRegistry` in the analysis
        // crate), so it renders named types as the `?type#<raw>`
        // placeholder. Callers that need FQN rendering should use a
        // project-aware Display.
        TypeKind::Type(d) => format!("?type#{}", d.raw()),
        TypeKind::Generic { decl, args } => {
            let parts: Box<[String]> = args
                .iter()
                .map(|a| display_fqn(arena, symbols, *a, home_lib))
                .collect();
            format!("?type#{}<{}>", decl.raw(), parts.join(", "))
        }
        // P35.3 — unresolved name, render verbatim with the same
        // `<lib>::` prefix the rest of the resolver would have used.
        TypeKind::Unresolved { name, .. } => {
            let name = &symbols[*name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::GenericParam { name, .. } => symbols[*name].to_string(),
        TypeKind::Lambda { params, ret } => {
            let parts: Box<[String]> = params
                .iter()
                .map(|p| display_fqn(arena, symbols, *p, home_lib))
                .collect();
            format!(
                "({}) -> {}",
                parts.join(", "),
                display_fqn(arena, symbols, *ret, home_lib)
            )
        }
        TypeKind::Enum { name, .. } => {
            let name = &symbols[*name];
            format!(
                "{}::{name}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
            )
        }
        TypeKind::Union { alts } => {
            // Same union rule as [`display`]: render with `|`-joined
            // alts and expand nullability into an explicit `null` alt
            // rather than appending a `?` suffix (which would read as
            // "only the last alt is nullable").
            let mut parts: Vec<String> = alts
                .iter()
                .map(|a| display_fqn(arena, symbols, *a, home_lib))
                .collect();
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                parts.push("null".to_string());
            }
            parts.join(" | ")
        }
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct TextCx {
        arena: TypeArena,
        symbols: SymbolTable,
    }

    #[test]
    fn intern_collapses_equal_types() {
        let mut cx = TextCx::default();
        let i1 = cx.arena.primitive(Primitive::Int);
        let i2 = cx.arena.primitive(Primitive::Int);
        assert_eq!(i1, i2);
        assert_eq!(cx.arena.len(), 1);
    }

    // P25.4
    /// `TypeKind` name fields are `SmolStr`. The arena's intern map
    /// keys on `Type` (which derives Hash + Eq), so two equivalent
    /// `Type` values constructed via different name-source paths must
    /// hash and compare equal. `SmolStr::hash` and `String::hash` both
    /// delegate to `str::hash`, so generic instantiations minted from
    /// a `String`-flavoured callsite (`String::from("Array").into()`)
    /// and from a `SmolStr`-flavoured callsite (`SmolStr::from("Array")`)
    /// must collapse to the same TypeId.
    #[test]
    fn typekind_name_dedups_across_smolstr_and_string_paths() {
        let mut cx = TextCx::default();
        let arg_string = cx.arena.primitive(Primitive::Int);
        let array_decl = TypeDeclId::from_raw(0);
        let g_a = cx.arena.generic(array_decl, vec![arg_string]);
        let g_b = cx.arena.generic(array_decl, vec![arg_string]);
        assert_eq!(g_a, g_b);
    }

    #[test]
    fn nullable_idempotent() {
        let mut cx = TextCx::default();
        let i = cx.arena.primitive(Primitive::Int);
        let q1 = cx.arena.nullable(i);
        let q2 = cx.arena.nullable(q1);
        assert_eq!(q1, q2);
    }

    #[test]
    fn primitives_do_not_cross_widen() {
        // P12.4: the GreyCat runtime rejects every primitive-to-primitive
        // widening at parameter / binding sites — including `int → float`,
        // which the TS reference checker permits. Verified live via
        // `greycat run`: `var i: int = 1; take(i)` against
        // `take(_: float)` is rejected. Identity is the only flow.
        let mut cx = TextCx::default();
        let i = cx.arena.primitive(Primitive::Int);
        let f = cx.arena.primitive(Primitive::Float);
        let s = cx.arena.primitive(Primitive::String);
        let c = cx.arena.primitive(Primitive::Char);
        assert!(!is_assignable_to(&cx.arena, i, f));
        assert!(!is_assignable_to(&cx.arena, f, i));
        assert!(!is_assignable_to(&cx.arena, c, i));
        assert!(!is_assignable_to(&cx.arena, i, c));
        assert!(!is_assignable_to(&cx.arena, c, s));
        assert!(!is_assignable_to(&cx.arena, s, c));
        assert!(is_assignable_to(&cx.arena, i, i));
        assert!(is_assignable_to(&cx.arena, f, f));
    }

    #[test]
    fn null_flows_into_nullable_only() {
        let mut cx = TextCx::default();
        let null = cx.arena.null();
        let int = cx.arena.primitive(Primitive::Int);
        let int_q = cx.arena.nullable(int);
        assert!(is_assignable_to(&cx.arena, null, int_q));
        assert!(!is_assignable_to(&cx.arena, null, int));
    }

    #[test]
    fn nullable_does_not_silently_narrow() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let int_q = cx.arena.nullable(int);
        assert!(is_assignable_to(&cx.arena, int, int_q));
        assert!(!is_assignable_to(&cx.arena, int_q, int));
    }

    #[test]
    fn any_top_never_bottom() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let any = cx.arena.any();
        let never = cx.arena.never();
        assert!(is_assignable_to(&cx.arena, int, any));
        assert!(is_assignable_to(&cx.arena, never, int));
    }

    #[test]
    fn generic_invariant_in_args() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let float = cx.arena.primitive(Primitive::Float);
        let array_decl = TypeDeclId::from_raw(0);
        let arr_int = cx.arena.generic(array_decl, vec![int]);
        let arr_float = cx.arena.generic(array_decl, vec![float]);
        // P12.2 (matches the GreyCat runtime, *not* the TS reference
        // checker): generic args are invariant. Even though `int`
        // widens to `float`, `Array<int>` is **not** assignable to
        // `Array<float>` (the runtime rejects this — we trust the
        // runtime as the oracle). The reverse is also rejected.
        assert!(!is_assignable_to(&cx.arena, arr_int, arr_float));
        assert!(!is_assignable_to(&cx.arena, arr_float, arr_int));
        assert!(is_assignable_to(&cx.arena, arr_int, arr_int));
    }

    #[test]
    fn generic_name_mismatch_stays_unassignable() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let array_decl = TypeDeclId::from_raw(0);
        let set_decl = TypeDeclId::from_raw(1);
        let arr_int = cx.arena.generic(array_decl, vec![int]);
        let set_int = cx.arena.generic(set_decl, vec![int]);
        // Different generic names with the same args still mismatch.
        // Inheritance-aware assignability (`type Child<T> extends
        // Parent<T>`) is a later phase.
        assert!(!is_assignable_to(&cx.arena, arr_int, set_int));
    }

    #[test]
    fn lambda_with_any_slot_is_symmetric() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let any = cx.arena.any();
        // After P20.1, `any` is interchangeable with any other type
        // (both top *and* bottom in the lattice — mirrors the runtime
        // which compiles `any → T` and defers the type check). So a
        // lambda with `any` in any slot is mutually assignable with a
        // lambda that has a concrete type in the same slot.
        // `f1: (any) -> int` ↔ `f2: (int) -> any`:
        //   * f1 → f2: param needs `int → any` ✓, return needs `int → any` ✓.
        //   * f2 → f1: param needs `any → int` ✓ (P20.1), return needs `any → int` ✓.
        let f1 = cx.arena.lambda(vec![any], int);
        let f2 = cx.arena.lambda(vec![int], any);
        assert!(is_assignable_to(&cx.arena, f1, f2));
        assert!(is_assignable_to(&cx.arena, f2, f1));
    }

    #[test]
    fn lambda_arity_mismatch_rejected() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        // Arity mismatch is hard-rejected regardless of the `any`
        // bidirectionality from P20.1 — no slot count, no relation.
        let f1 = cx.arena.lambda(vec![int], int);
        let f2 = cx.arena.lambda(vec![int, int], int);
        assert!(!is_assignable_to(&cx.arena, f1, f2));
        assert!(!is_assignable_to(&cx.arena, f2, f1));
    }

    #[test]
    fn union_member_flows_in() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let str_t = cx.arena.primitive(Primitive::String);
        let union = cx.arena.alloc(Type {
            kind: TypeKind::Union {
                alts: vec![int, str_t],
            },
            nullable: false,
        });
        assert!(is_assignable_to(&cx.arena, int, union));
        assert!(is_assignable_to(&cx.arena, str_t, union));
        let bool_t = cx.arena.primitive(Primitive::Bool);
        assert!(!is_assignable_to(&cx.arena, bool_t, union));
    }

    #[test]
    fn registry_lookup() {
        let mut cx = TextCx::default();
        let mut reg = TypeRegistry::new();
        let foo_decl = TypeDeclId::from_raw(0);
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
    fn display_renders_nullable_suffix() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let int_q = cx.arena.nullable(int);
        let str_t = cx.arena.primitive(Primitive::String);
        let array_decl = TypeDeclId::from_raw(0);
        let arr = cx.arena.generic(array_decl, vec![str_t]);
        assert_eq!(cx.arena.display(int_q, &cx.symbols).to_string(), "int?");
        // Core's bare `display` no longer knows decl names — the arena
        // doesn't store them. `Array<String>`-style rendering lives in
        // the analysis crate's `display_type` helper, which threads a
        // `DeclRegistry` through.
        assert_eq!(
            cx.arena.display(arr, &cx.symbols).to_string(),
            "?type#0<String>"
        );
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
        let person_decl = TypeDeclId::from_raw(1);
        let person = cx.arena.alloc_type(person_decl);
        let node_decl = TypeDeclId::from_raw(0);
        let node_person = cx.arena.generic(node_decl, vec![person]);
        assert!(!is_assignable_to(&cx.arena, node_person, person));
        assert!(!is_assignable_to(&cx.arena, person, node_person));
    }

    #[test]
    fn inference_table_substitutes_generic_params() {
        let mut cx = TextCx::default();
        let t_sym = cx.symbols.intern("T");
        let foo_sym = cx.symbols.intern("Foo");
        let int = cx.arena.primitive(Primitive::Int);
        let t_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam {
                name: t_sym,
                owner: GenericOwner::Type(foo_sym),
            },
            nullable: false,
        });
        let array_decl = TypeDeclId::from_raw(0);
        let arr_t = cx.arena.generic(array_decl, vec![t_param]);

        let mut tbl = InferenceTable::new();
        tbl.bind(t_sym, int);

        let resolved = tbl.substitute(&mut cx.arena, arr_t);
        let resolved_kind = &cx.arena.get(resolved).kind;
        let TypeKind::Generic { decl, args } = resolved_kind else {
            panic!("expected Array<int>, got {resolved_kind:?}");
        };
        assert_eq!(*decl, array_decl);
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int]);
    }

    #[test]
    fn arena_substitute_replaces_generic_params() {
        let mut cx = TextCx::default();
        let t_sym = cx.symbols.intern("T");
        let u_sym = cx.symbols.intern("U");
        let foo_sym = cx.symbols.intern("Foo");
        let int = cx.arena.primitive(Primitive::Int);
        let str_t = cx.arena.primitive(Primitive::String);
        let t_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam {
                name: t_sym,
                owner: GenericOwner::Type(foo_sym),
            },
            nullable: false,
        });
        let u_param = cx.arena.alloc(Type {
            kind: TypeKind::GenericParam {
                name: u_sym,
                owner: GenericOwner::Type(foo_sym),
            },
            nullable: false,
        });
        let array_decl = TypeDeclId::from_raw(0);
        let map_decl = TypeDeclId::from_raw(1);
        let map_tu = cx.arena.generic(map_decl, vec![t_param, u_param]);

        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        subst.insert(t_sym, int);
        subst.insert(u_sym, str_t);

        let resolved = cx.arena.substitute(map_tu, &subst);
        let TypeKind::Generic { decl, args } = &cx.arena.get(resolved).kind else {
            panic!("expected Map<int, String>");
        };
        assert_eq!(*decl, map_decl);
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int, str_t]);

        // Idempotent: re-applying yields the same TypeId.
        let resolved2 = cx.arena.substitute(resolved, &subst);
        assert_eq!(resolved, resolved2);

        // Nullability preserved: Array<T?> with T → int gives Array<int?>.
        let t_param_q = cx.arena.nullable(t_param);
        let arr_t_q = cx.arena.generic(array_decl, vec![t_param_q]);
        let resolved_q = cx.arena.substitute(arr_t_q, &subst);
        let TypeKind::Generic { args: q_args, .. } = &cx.arena.get(resolved_q).kind else {
            panic!();
        };
        assert!(cx.arena.get(q_args[0]).nullable);
    }

    #[test]
    fn arena_substitute_no_op_on_empty_subst() {
        let mut cx = TextCx::default();
        let int = cx.arena.primitive(Primitive::Int);
        let array_decl = TypeDeclId::from_raw(0);
        let arr = cx.arena.generic(array_decl, vec![int]);
        let empty: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        assert_eq!(cx.arena.substitute(arr, &empty), arr);
    }
}
