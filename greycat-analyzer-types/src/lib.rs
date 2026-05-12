//! Type system for greycat — foundation port.
//!
//! Ports the core of `packages/lang/src/analysis/types.ts` (~2,811 LoC of
//! TS). This crate is the foundation the analyzer builds on; it owns
//! the `Type` enum, type interning, and subtyping rules.
//!
//! What's here:
//! - [`Type`]: the central enum (primitives, named, generic, lambda, etc.)
//! - [`TypeId`]: a `Copy` handle into the [`TypeArena`].
//! - Primitive type ids (`null_t()`, `int_t()`, ...) for cheap comparisons.
//! - [`TypeRegistry`]: holds per-module declared types so Named lookups
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
use smol_str::SmolStr;

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

// P19.9
/// A project-wide interned identifier. `Copy`-able 32-bit
/// handle into a [`SymbolTable`]; comparing two `Symbol`s is one
/// integer compare regardless of source string length.
///
/// `Symbol`s are *not* comparable across `SymbolTable` instances
/// each table assigns its own dense numbering. The
/// [`crate::SymbolTable`] that issued a symbol must be the one used
/// to resolve it back to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(u32);

impl Symbol {
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

// P19.9
/// Append-only string interner. One allocation per unique
/// name across the project lifetime. Hot lookup paths (analyzer body
/// walker, project orchestrator) use `lookup` for read-only checks
/// and `intern` only when extending the index.
#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    map: FxHashMap<String, Symbol>,
    rev: Vec<String>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `name`. Idempotent — the second call with the same
    /// `name` returns the same [`Symbol`] without allocating.
    pub fn intern(&mut self, name: &str) -> Symbol {
        if let Some(&sym) = self.map.get(name) {
            return sym;
        }
        let sym = Symbol(self.rev.len() as u32);
        let owned = name.to_string();
        self.rev.push(owned.clone());
        self.map.insert(owned, sym);
        sym
    }

    /// Read-only lookup: returns the existing [`Symbol`] for `name`
    /// or `None` if no one has interned it yet. Use this in hot
    /// lookup paths where adding a stale entry would be incorrect.
    pub fn lookup(&self, name: &str) -> Option<Symbol> {
        self.map.get(name).copied()
    }

    /// Resolve `sym` back to its text. Returns `None` if `sym` came
    /// from a different table (or is otherwise out of bounds).
    pub fn resolve(&self, sym: Symbol) -> Option<&str> {
        self.rev.get(sym.0 as usize).map(|s| s.as_str())
    }

    pub fn len(&self) -> usize {
        self.rev.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rev.is_empty()
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
    /// `any` — top type. Anything is assignable to it.
    Any,
    /// `never` — bottom type. Used for unreachable code.
    Never,
    /// Named primitive — `int`, `float`, `String`, `bool`, `char`,
    /// `time`, `duration`, `geo`. Carries the canonical name.
    Primitive(Primitive),
    /// **Deprecated** — being replaced by [`TypeKind::Type`] and
    /// [`TypeKind::Unresolved`]. Retained while the migration lands
    /// chunk by chunk.
    ///
    /// Named user / stdlib type, identified by its fully-qualified name
    /// (`<lib>::<module>::<TypeName>` or just `<TypeName>` until we wire
    /// fully-qualified resolution).
    // P25.4 / P35
    Named { name: SmolStr },
    /// **Deprecated** — being replaced by [`TypeKind::GenericInstance`].
    ///
    /// Generic type instantiation — `Array<int>`, `Map<String, int>`, etc.
    // P25.4 / P25.7 / P35
    Generic {
        name: SmolStr,
        args: SmallVec<[TypeId; 2]>,
    },
    // P35.2
    /// A resolved non-generic type — user-defined `type Foo {...}` or
    /// a non-generic native type from `std/core`. The decl handle is
    /// the identity; cross-module references to the same decl share
    /// the same `TypeDeclId`, so equality is one register-sized
    /// compare.
    ///
    /// Distinct from [`TypeKind::GenericInstance`] with empty args.
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
    GenericInstance {
        decl: TypeDeclId,
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
        name: SmolStr,
        byte_range: (usize, usize),
    },
    /// Generic type *parameter* — the `T` inside a `fn<T>(x: T)` body.
    // P25.4
    GenericParam { name: SmolStr, owner: GenericOwner },
    /// Function / lambda type.
    Lambda(LambdaType),
    /// Enum type.
    // P25.4
    Enum {
        name: SmolStr,
        variants: Vec<SmolStr>,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LambdaType {
    pub params: Vec<TypeId>,
    pub ret: TypeId,
}

/// Where a generic parameter was declared.
// P25.4
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GenericOwner {
    /// `fn<T>(...)`.
    Function(SmolStr),
    /// `type Foo<T> {...}`.
    Type(SmolStr),
}

// =============================================================================
// Arena
// =============================================================================

/// Append-only interning arena for `Type`. Two equal `Type` values get
/// the same [`TypeId`]; comparing for equality is then just an integer
/// comparison.
///
/// The arena also owns a parallel `decl_names` table indexed by
/// `TypeDeclId.raw()`. Every `alloc_type` / `alloc_generic_instance`
/// records the decl's source name there, so `arena.display(id)` can
/// recover a printable name without a callback or a borrow on the
/// project's `DeclRegistry`. The duplication is a deliberate
/// denormalisation: `DeclRegistry` stays the canonical `(uri, decl) →
/// handle` interner; `decl_names` is a read-path view scoped to one
/// arena.
#[derive(Debug, Default, Clone)]
pub struct TypeArena {
    items: Vec<Type>,
    intern: FxHashMap<Type, TypeId>,
    decl_names: Vec<SmolStr>,
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

    pub fn named(&mut self, name: impl Into<SmolStr>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Named { name: name.into() },
            nullable: false,
        })
    }

    pub fn generic(&mut self, name: impl Into<SmolStr>, args: Vec<TypeId>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Generic {
                name: name.into(),
                // P25.7
                args: args.into(),
            },
            nullable: false,
        })
    }

    // P35.2
    /// Allocate a resolved non-generic [`TypeKind::Type`]. Registers
    /// `name` with the arena so [`Self::display`] can render the type
    /// without a callback or a `DeclRegistry` borrow. Name registration
    /// is idempotent on `decl`.
    pub fn alloc_type(&mut self, decl: TypeDeclId, name: impl Into<SmolStr>) -> TypeId {
        self.register_decl_name(decl, name.into());
        self.alloc(Type {
            kind: TypeKind::Type(decl),
            nullable: false,
        })
    }

    // P35.2
    /// Allocate a [`TypeKind::GenericInstance`]. Caller guarantees
    /// `args` is non-empty — zero-arg uses of a generic decl are an
    /// upstream lowering error, not a value-shaped concept. `name` is
    /// registered with the arena (same idempotency as
    /// [`Self::alloc_type`]).
    pub fn alloc_generic_instance(
        &mut self,
        decl: TypeDeclId,
        name: impl Into<SmolStr>,
        args: Vec<TypeId>,
    ) -> TypeId {
        debug_assert!(!args.is_empty(), "GenericInstance must have non-empty args");
        self.register_decl_name(decl, name.into());
        self.alloc(Type {
            kind: TypeKind::GenericInstance {
                decl,
                args: args.into(),
            },
            nullable: false,
        })
    }

    /// Idempotently record `name` as the printable source name for
    /// `decl`. First call wins; subsequent calls with a matching name
    /// no-op, conflicting names trip a debug-assert (the decl identity
    /// is supposed to be 1:1 with its name).
    fn register_decl_name(&mut self, decl: TypeDeclId, name: SmolStr) {
        let i = decl.raw() as usize;
        if i >= self.decl_names.len() {
            self.decl_names.resize(i + 1, SmolStr::default());
        }
        if self.decl_names[i].is_empty() {
            self.decl_names[i] = name;
        } else {
            debug_assert_eq!(
                self.decl_names[i],
                name,
                "TypeDeclId {} re-registered with a different name",
                decl.raw()
            );
        }
    }

    /// Recover the source name for `decl`, or `None` if no
    /// `alloc_type` / `alloc_generic_instance` has interned it yet.
    pub fn decl_name(&self, decl: TypeDeclId) -> Option<&str> {
        self.decl_names
            .get(decl.raw() as usize)
            .filter(|s| !s.is_empty())
            .map(SmolStr::as_str)
    }

    /// Return a [`Display`]-implementing wrapper that renders the type
    /// at `id`. Reads decl names from this arena's `decl_names` table —
    /// no callback, no registry borrow.
    pub fn display(&self, id: TypeId) -> TypeDisplay<'_> {
        TypeDisplay { arena: self, id }
    }

    // P35.3
    /// Allocate a [`TypeKind::Unresolved`]. Use this in place of the
    /// `arena.any()` fallback when a type-ref name didn't resolve —
    /// behaves like `any` for assignability but carries the source
    /// name + span for diagnostic rendering. Nullable to match
    /// `any`'s semantics: an unresolved name has no constraint
    /// against null.
    pub fn unresolved(&mut self, name: impl Into<SmolStr>, byte_range: (usize, usize)) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Unresolved {
                name: name.into(),
                byte_range,
            },
            nullable: true,
        })
    }

    pub fn generic_param(&mut self, name: impl Into<SmolStr>, owner: GenericOwner) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::GenericParam {
                name: name.into(),
                owner,
            },
            nullable: false,
        })
    }

    pub fn lambda(&mut self, params: Vec<TypeId>, ret: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Lambda(LambdaType { params, ret }),
            nullable: false,
        })
    }

    /// `(x, y)` tuple-literal type, modelled as `Tuple<X, Y>` per
    /// the compiler's desugaring rule (mirrors `[42]` ≡
    /// `Array<int>{42}`). Strictly 2-element — the grammar's
    /// `tuple_expr` rule emits exactly `(left, right)` and nothing
    /// else, so the type is always a pair.
    pub fn tuple(&mut self, x: TypeId, y: TypeId) -> TypeId {
        self.generic("Tuple", vec![x, y])
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
    /// `Union` shapes. Non-substitutable kinds (`Named`, `Primitive`,
    /// `Null`, `Any`, `Never`, `Enum`) return `ty` unchanged.
    pub fn substitute(&mut self, ty: TypeId, subst: &FxHashMap<String, TypeId>) -> TypeId {
        if subst.is_empty() {
            return ty;
        }
        let t = self.get(ty).clone();
        match &t.kind {
            TypeKind::GenericParam { name, .. } => match subst.get(name.as_str()) {
                Some(&witness) if t.nullable => self.nullable(witness),
                Some(&witness) => witness,
                None => ty,
            },
            TypeKind::Generic { name, args } => {
                // P25.7
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(*a, subst)).collect();
                if new_args == *args {
                    ty
                } else {
                    let name = name.clone();
                    let mut new_t = self.generic(name, new_args.into_vec());
                    if t.nullable {
                        new_t = self.nullable(new_t);
                    }
                    new_t
                }
            }
            // P35.2 — `Type(decl)` is non-generic, no params to
            // substitute. `GenericInstance` mirrors `Generic`.
            TypeKind::Type(_) => ty,
            TypeKind::GenericInstance { decl, args } => {
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(*a, subst)).collect();
                if new_args == *args {
                    ty
                } else {
                    let decl = *decl;
                    // Re-use the name already registered for `decl` when
                    // we minted the original `GenericInstance` — no
                    // caller-side bookkeeping needed.
                    let name = SmolStr::from(
                        self.decl_name(decl)
                            .expect("decl name registered at first alloc"),
                    );
                    let mut new_t = self.alloc_generic_instance(decl, name, new_args.into_vec());
                    if t.nullable {
                        new_t = self.nullable(new_t);
                    }
                    new_t
                }
            }
            TypeKind::Lambda(l) => {
                let new_params: Vec<TypeId> = l
                    .params
                    .iter()
                    .map(|p| self.substitute(*p, subst))
                    .collect();
                let new_ret = self.substitute(l.ret, subst);
                if new_params == l.params && new_ret == l.ret {
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

/// Looks up named types. / will populate this from HIR + stdlib.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    /// Maps simple type name -> a Named TypeId in the arena.
    // P25.2
    named: FxHashMap<SmolStr, TypeId>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: impl Into<SmolStr>, id: TypeId) {
        self.named.insert(name.into(), id);
    }

    pub fn lookup(&self, name: &str) -> Option<TypeId> {
        self.named.get(name).copied()
    }

    // P19.6
    /// Iterate every registered name. Used by the
    /// signature-cache invalidation path to fingerprint the
    /// project-wide name set.
    pub fn iter_names(&self) -> impl Iterator<Item = &str> {
        self.named.keys().map(|s| s.as_str())
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

    // P7.3 node tagging: `node<T>` / `nodeTime<T>` / etc. auto-deref to
    // their inner `T`. The reverse direction stays asymmetric — a bare
    // `T` cannot promote to a tagged-node form without an explicit
    // constructor call.
    if let TypeKind::Generic { name, args } = &a.kind
        && is_node_tag(name)
        && args.len() == 1
        && is_assignable_to(arena, args[0], to)
    {
        return true;
    }

    match (&a.kind, &b.kind) {
        (TypeKind::Primitive(pa), TypeKind::Primitive(pb)) => primitive_assignable(*pa, *pb),
        // `(Named, Named)` arm removed: production paths now mint
        // `Type(handle)` for every `.gcl`-declared type via
        // `ProjectIndex::ingest`, so `Named` only survives for
        // genuinely-unknown names — which `is_assignable_to`'s
        // Unresolved arm already handles upstream.
        (TypeKind::Generic { name: na, args: aa }, TypeKind::Generic { name: nb, args: ab }) => {
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
            // `is_assignable_to` rather than raw `TypeId ==`. The
            // two are equivalent for primitives and Named-vs-Named
            // (the only widening rule is `int <: float`, which is
            // not symmetric, so primitives still test as distinct
            // unless their `TypeId`s are identical). The bidirectional
            // form is what lets `Map<Enum{Target,...}, V>` and
            // `Map<Named{Target}, V>` count as the same outer type
            // — the Named<->Enum identity at lines 565-566 returns
            // `true` in both directions, so arg-equality recovers.
            // Without this, two paths that lower the same enum-typed
            // arg differently (analyzer's `lower_type_ref` produces
            // `Enum{...}`, the validation pass's `mint_type_shape`
            // produces `Named{...}`) would diverge in the containing
            // `Generic` and surface false-positive
            // "value of `Map<Target, V>` not assignable to parameter
            // `_: Map<Target, V>`" diagnostics.
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
            if na == nb
                && aa.len() == ab.len()
                && !ab.is_empty()
                && ab
                    .iter()
                    .all(|y| matches!(arena.get(*y).kind, TypeKind::Any))
            {
                return true;
            }
            // Node-tag generics (`node`, `nodeTime`, `nodeList`, `nodeIndex`,
            // `nodeGeo`) are bivariant on their inner args at runtime: a node
            // ref is a 64-bit handle and the runtime accepts e.g.
            // `nodeTime<float>` → `nodeTime<float?>` (and the reverse),
            // `nodeList<node<Dog>>` → `nodeList<node<Animal>>`, etc. Verified
            // against the runtime oracle. Outer-name equality + arg arity are
            // still required.
            if na == nb && aa.len() == ab.len() && is_node_tag(na) {
                return true;
            }
            na == nb
                && aa.len() == ab.len()
                && aa.iter().zip(ab).all(|(x, y)| {
                    if *x == *y {
                        return true;
                    }
                    is_assignable_to(arena, *x, *y) && is_assignable_to(arena, *y, *x)
                })
        }
        (TypeKind::Lambda(la), TypeKind::Lambda(lb)) => {
            // Contravariant in params, covariant in return. Same as TS.
            la.params.len() == lb.params.len()
                && la
                    .params
                    .iter()
                    .zip(&lb.params)
                    .all(|(p_a, p_b)| is_assignable_to(arena, *p_b, *p_a))
                && is_assignable_to(arena, la.ret, lb.ret)
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
        // Cross-arena enum identity: when one side resolves to the
        // registered `Enum { name, variants }` shape and the other
        // crossed an arena boundary as a bare `Named { name }` (the
        // post-pass mints param types via `mint_type_shape`, which
        // produces `Named` for any non-builtin name without consulting
        // the home module's registry), treat them as the same type
        // when names agree. Otherwise an enum value flowing into an
        // enum-typed slot lights up "value of type `Foo` is not
        // assignable to parameter `_: Foo`" false positives.
        (TypeKind::Enum { name: na, .. }, TypeKind::Named { name: nb })
        | (TypeKind::Named { name: nb }, TypeKind::Enum { name: na, .. }) => na == nb,
        // P35.2 — decl-handle identity. `Type(decl)` and
        // `GenericInstance { decl, .. }` compare by handle equality;
        // generic args reuse the same invariance / node-tag bivariance
        // / all-any-wildcard rules as the SmolStr `Generic` arm above.
        // Node-tag dispatch here still goes through `is_node_tag(name)`
        // because no caller mints `GenericInstance` yet — 35.5 switches
        // the node-tag check to a handle comparison once `WellKnown` is
        // threaded into this fn.
        (TypeKind::Type(da), TypeKind::Type(db)) => da == db,
        (
            TypeKind::GenericInstance { decl: da, args: aa },
            TypeKind::GenericInstance { decl: db, args: ab },
        ) => {
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

/// Primitive widening lattice: `int -> float`, plus identity. Strings,
/// chars, bools etc. don't widen.
/// `true` for any of the runtime "node-tag" generic names that
/// auto-deref to their inner type in the assignability relation
///. Drawn from the TS reference's `StdCoreTypes` interface.
///
/// **Deprecated** — string-keyed identity for the node-tag family
/// lets a user-declared `type node<T>` impersonate the std-core tag
/// and pick up auto-deref / bivariance semantics it shouldn't.
/// Migrate to `WellKnown::is_node_tag(decl)` in
/// [`greycat_analyzer_analysis::well_known`] (handle-keyed) as
/// callers switch to `TypeKind::GenericInstance { decl, args }`.
/// This function only dispatches against the legacy
/// `TypeKind::Named` / `TypeKind::Generic` variants.
// P35.5
pub fn is_node_tag(name: &str) -> bool {
    matches!(
        name,
        "node" | "nodeTime" | "nodeGeo" | "nodeList" | "nodeIndex"
    )
}

// =============================================================================
// Inference table (P7.4 — foundational pass)
// =============================================================================

/// Per-call constraint table that records "type-parameter `T` was
/// witnessed at type `…`" pairs as the analyzer walks a generic call
/// site. After all arguments have been visited, [`InferenceTable::solve`]
/// substitutes accumulated witnesses into the declared return type.
///
/// **Scope:** records and substitutes simple `GenericParam` ↔ concrete
/// pairs. Constraint propagation (e.g. `T : SomeBound` requiring the
/// witness to satisfy the bound), variance handling beyond what
/// [`is_assignable_to`] already provides, and union-of-witnesses
/// merging are deferred — this is the seam, not a full Hindley-Milner.
#[derive(Debug, Default)]
pub struct InferenceTable {
    bindings: FxHashMap<String, TypeId>,
}

impl InferenceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a witness for a generic param. If the same param has
    /// already been bound, the new witness is dropped — the analyzer's
    /// caller should already have type-checked it against the prior
    /// witness through [`is_assignable_to`].
    pub fn bind(&mut self, name: impl Into<String>, ty: TypeId) {
        self.bindings.entry(name.into()).or_insert(ty);
    }

    pub fn lookup(&self, name: &str) -> Option<TypeId> {
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
                if let Some(witness) = self.bindings.get(name.as_str()) {
                    let nullable = t.nullable;
                    if !nullable {
                        return *witness;
                    }
                    arena.nullable(*witness)
                } else {
                    ty
                }
            }
            TypeKind::Generic { name, args } => {
                // P25.7
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(arena, *a)).collect();
                if new_args == *args {
                    ty
                } else {
                    let name = name.clone();
                    let mut new_t = arena.generic(name, new_args.into_vec());
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
/// - `int ↔ {int, float, node, nodeTime, nodeList, nodeIndex, nodeGeo}`.
/// - `float ↔ {int, float}`.
/// - `node{,Time,List,Index,Geo} ↔ {self, int}`.
/// - `String ↔ String`.
/// - `char ↔ {char, String, int}`.
/// - `bool ↔ bool`.
/// - `t{2,3,4}{,f} → int`.
/// - Enums → `int`.
/// - Anything else falls through to "same head name OR `from` assignable
///   to `to` (no inheritance check yet — that lands when supertype
///   chains thread through the analyzer)".
pub fn is_castable(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
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

    // Union: cast iff any alt casts. Source nullability is otherwise
    // ignored — the TS reference's `from = from.nn()` strip is purely
    // about treating `T?` like `T` for kind dispatch, which we get
    // for free by reading `from_t.kind` directly.
    if let TypeKind::Union { alts } = &from_t.kind {
        return alts.iter().any(|a| is_castable(arena, *a, to));
    }
    if matches!(from_t.kind, TypeKind::Enum { .. }) && is_int_target(to_t) {
        return true;
    }

    let to_head = generic_or_named_name(to_t);
    match &from_t.kind {
        TypeKind::Any => true,
        // **P19.14** — `T as Foo` (where `T` is a generic param)
        // is allowed: the runtime decides at instantiation time.
        // Same for the symmetric `Foo as T` direction.
        TypeKind::GenericParam { .. } => true,
        TypeKind::Primitive(Primitive::Int) => {
            matches!(
                to_head.as_deref(),
                Some("node" | "nodeTime" | "nodeList" | "nodeIndex" | "nodeGeo")
            ) || is_primitive(to_t, Primitive::Int)
                || is_primitive(to_t, Primitive::Float)
        }
        TypeKind::Primitive(Primitive::Float) => {
            is_primitive(to_t, Primitive::Int) || is_primitive(to_t, Primitive::Float)
        }
        TypeKind::Primitive(Primitive::String) => is_primitive(to_t, Primitive::String),
        TypeKind::Primitive(Primitive::Char) => {
            is_primitive(to_t, Primitive::Char)
                || is_primitive(to_t, Primitive::String)
                || is_primitive(to_t, Primitive::Int)
        }
        TypeKind::Primitive(Primitive::Bool) => is_primitive(to_t, Primitive::Bool),
        // node-tag heads: cast to int or to themselves (covariant
        // generic args via the `same head name` branch — narrows are
        // P12.4 territory).
        TypeKind::Generic { name, .. } | TypeKind::Named { name } if is_node_tag(name) => {
            is_int_target(to_t) || matches!(to_head.as_deref(), Some(n) if n == name)
        }
        // Tuple primitives → int.
        TypeKind::Generic { name, .. } | TypeKind::Named { name }
            if matches!(name.as_str(), "t2" | "t3" | "t4" | "t2f" | "t3f" | "t4f") =>
        {
            is_int_target(to_t)
        }
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
    // shapes for cast-fallthrough are Named/Enum identity and primitive
    // widening — broader generic / lambda compatibility is rare in the
    // `as` position but we delegate to `is_assignable_to` after the
    // shape match.
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
        (TypeKind::Named { name: na }, TypeKind::Named { name: nb }) if na == nb => true,
        (TypeKind::Enum { name: na, .. }, TypeKind::Enum { name: nb, .. }) if na == nb => true,
        (TypeKind::Named { name: na }, TypeKind::Enum { name: nb, .. })
        | (TypeKind::Enum { name: na, .. }, TypeKind::Named { name: nb })
            if na == nb =>
        {
            true
        }
        (TypeKind::Primitive(pa), TypeKind::Primitive(pb)) => primitive_assignable(*pa, *pb),
        _ => false,
    }
}

fn generic_or_named_name(t: &Type) -> Option<SmolStr> {
    match &t.kind {
        TypeKind::Generic { name, .. } | TypeKind::Named { name } => Some(name.clone()),
        TypeKind::Primitive(p) => Some(p.name().into()),
        _ => None,
    }
}

fn is_primitive(t: &Type, p: Primitive) -> bool {
    matches!(t.kind, TypeKind::Primitive(q) if q == p)
}

fn is_int_target(t: &Type) -> bool {
    is_primitive(t, Primitive::Int)
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
/// printable names for [`TypeKind::Type`] / [`TypeKind::GenericInstance`].
/// When a decl handle hasn't been registered yet (an internal invariant
/// violation, since every alloc registers a name), falls back to the
/// `?type#<raw>` placeholder so the output stays distinguishable.
///
/// Writes straight into the formatter — no intermediate `String`.
pub struct TypeDisplay<'a> {
    arena: &'a TypeArena,
    id: TypeId,
}

impl std::fmt::Display for TypeDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type(f, self.arena, self.id)
    }
}

fn write_type(f: &mut std::fmt::Formatter<'_>, arena: &TypeArena, id: TypeId) -> std::fmt::Result {
    let ty = arena.get(id);
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Primitive(p) => f.write_str(p.name())?,
        TypeKind::Named { name } => f.write_str(name.as_str())?,
        TypeKind::Generic { name, args } => {
            f.write_str(name.as_str())?;
            write_args(f, arena, args)?;
        }
        TypeKind::Type(d) => match arena.decl_name(*d) {
            Some(name) => f.write_str(name)?,
            None => write!(f, "?type#{}", d.raw())?,
        },
        TypeKind::GenericInstance { decl, args } => {
            match arena.decl_name(*decl) {
                Some(name) => f.write_str(name)?,
                None => write!(f, "?type#{}", decl.raw())?,
            }
            write_args(f, arena, args)?;
        }
        TypeKind::Unresolved { name, .. } => f.write_str(name.as_str())?,
        TypeKind::GenericParam { name, .. } => f.write_str(name.as_str())?,
        TypeKind::Lambda(l) => {
            f.write_str("(")?;
            for (i, p) in l.params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type(f, arena, *p)?;
            }
            f.write_str(") -> ")?;
            write_type(f, arena, l.ret)?;
        }
        TypeKind::Enum { name, .. } => f.write_str(name.as_str())?,
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
                write_type(f, arena, *a)?;
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
    args: &[TypeId],
) -> std::fmt::Result {
    f.write_str("<")?;
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_type(f, arena, *a)?;
    }
    f.write_str(">")
}

// P18.1
/// Fully-qualified-name display, matching the GreyCat canonical
/// printer (e.g. `core::int`, `core::Array<core::int?>`,
/// `project::Foo`).
///
/// `home_lib` resolves a Named/Generic/Enum's home module (e.g. `Foo →
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
        TypeKind::Named { name } => format!(
            "{}::{}",
            home_lib(name.as_str()).unwrap_or_else(|| "core".to_string()),
            name
        ),
        TypeKind::Generic { name, args } => {
            let lib = home_lib(name.as_str()).unwrap_or_else(|| "core".to_string());
            let parts: Vec<String> = args
                .iter()
                .map(|a| display_fqn(arena, *a, home_lib))
                .collect();
            format!("{lib}::{name}<{}>", parts.join(", "))
        }
        // P35.2 — decl names come from the arena (registered at alloc
        // time). `home_lib` is still threaded for the legacy `Named` /
        // `Generic` shapes where the name isn't keyed by a decl handle.
        TypeKind::Type(d) => match arena.decl_name(*d) {
            Some(name) => format!(
                "{}::{}",
                home_lib(name).unwrap_or_else(|| "core".to_string()),
                name
            ),
            None => format!("?type#{}", d.raw()),
        },
        TypeKind::GenericInstance { decl, args } => {
            let parts: Vec<String> = args
                .iter()
                .map(|a| display_fqn(arena, *a, home_lib))
                .collect();
            match arena.decl_name(*decl) {
                Some(name) => format!(
                    "{}::{}<{}>",
                    home_lib(name).unwrap_or_else(|| "core".to_string()),
                    name,
                    parts.join(", ")
                ),
                None => format!("?type#{}<{}>", decl.raw(), parts.join(", ")),
            }
        }
        // P35.3 — unresolved name, render verbatim with the same
        // `<lib>::` prefix the rest of the resolver would have used.
        TypeKind::Unresolved { name, .. } => format!(
            "{}::{}",
            home_lib(name.as_str()).unwrap_or_else(|| "core".to_string()),
            name
        ),
        TypeKind::GenericParam { name, .. } => name.to_string(),
        TypeKind::Lambda(l) => {
            let parts: Vec<String> = l
                .params
                .iter()
                .map(|p| display_fqn(arena, *p, home_lib))
                .collect();
            format!(
                "({}) -> {}",
                parts.join(", "),
                display_fqn(arena, l.ret, home_lib)
            )
        }
        TypeKind::Enum { name, .. } => format!(
            "{}::{}",
            home_lib(name.as_str()).unwrap_or_else(|| "core".to_string()),
            name
        ),
        TypeKind::Union { alts } => {
            // Same union rule as [`display`]: render with `|`-joined
            // alts and expand nullability into an explicit `null` alt
            // rather than appending a `?` suffix (which would read as
            // "only the last alt is nullable").
            let mut parts: Vec<String> = alts
                .iter()
                .map(|a| display_fqn(arena, *a, home_lib))
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

    fn fresh() -> TypeArena {
        TypeArena::new()
    }

    #[test]
    fn intern_collapses_equal_types() {
        let mut a = fresh();
        let i1 = a.primitive(Primitive::Int);
        let i2 = a.primitive(Primitive::Int);
        assert_eq!(i1, i2);
        assert_eq!(a.len(), 1);
    }

    // P25.4
    /// `TypeKind` name fields are `SmolStr`. The arena's intern map
    /// keys on `Type` (which derives Hash + Eq), so two equivalent
    /// `Type` values constructed via different name-source paths must
    /// hash and compare equal. `SmolStr::hash` and `String::hash` both
    /// delegate to `str::hash`, so `arena.named("Foo")` from a
    /// `String`-flavoured callsite (`String::from("Foo").into()`) and
    /// from a `SmolStr`-flavoured callsite (`SmolStr::from("Foo")`)
    /// must collapse to the same TypeId. This test anchors that
    /// invariant so a future refactor that accidentally introduces a
    /// hashing-asymmetric variant gets caught.
    #[test]
    fn typekind_name_dedups_across_smolstr_and_string_paths() {
        let mut a = fresh();
        let from_string = a.named(String::from("Foo"));
        let from_smol = a.named(SmolStr::from("Foo"));
        let from_str = a.named("Foo");
        assert_eq!(from_string, from_smol);
        assert_eq!(from_smol, from_str);

        let arg_string = a.primitive(Primitive::Int);
        let g_a = a.generic(String::from("Array"), vec![arg_string]);
        let g_b = a.generic(SmolStr::from("Array"), vec![arg_string]);
        assert_eq!(g_a, g_b);
    }

    #[test]
    fn nullable_idempotent() {
        let mut a = fresh();
        let i = a.primitive(Primitive::Int);
        let q1 = a.nullable(i);
        let q2 = a.nullable(q1);
        assert_eq!(q1, q2);
    }

    #[test]
    fn primitives_do_not_cross_widen() {
        // P12.4: the GreyCat runtime rejects every primitive-to-primitive
        // widening at parameter / binding sites — including `int → float`,
        // which the TS reference checker permits. Verified live via
        // `greycat run`: `var i: int = 1; take(i)` against
        // `take(_: float)` is rejected. Identity is the only flow.
        let mut a = fresh();
        let i = a.primitive(Primitive::Int);
        let f = a.primitive(Primitive::Float);
        let s = a.primitive(Primitive::String);
        let c = a.primitive(Primitive::Char);
        assert!(!is_assignable_to(&a, i, f));
        assert!(!is_assignable_to(&a, f, i));
        assert!(!is_assignable_to(&a, c, i));
        assert!(!is_assignable_to(&a, i, c));
        assert!(!is_assignable_to(&a, c, s));
        assert!(!is_assignable_to(&a, s, c));
        assert!(is_assignable_to(&a, i, i));
        assert!(is_assignable_to(&a, f, f));
    }

    #[test]
    fn null_flows_into_nullable_only() {
        let mut a = fresh();
        let null = a.null();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        assert!(is_assignable_to(&a, null, int_q));
        assert!(!is_assignable_to(&a, null, int));
    }

    #[test]
    fn nullable_does_not_silently_narrow() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        assert!(is_assignable_to(&a, int, int_q));
        assert!(!is_assignable_to(&a, int_q, int));
    }

    #[test]
    fn any_top_never_bottom() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let any = a.any();
        let never = a.never();
        assert!(is_assignable_to(&a, int, any));
        assert!(is_assignable_to(&a, never, int));
    }

    #[test]
    fn generic_invariant_in_args() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let float = a.primitive(Primitive::Float);
        let arr_int = a.generic("Array", vec![int]);
        let arr_float = a.generic("Array", vec![float]);
        // P12.2 (matches the GreyCat runtime, *not* the TS reference
        // checker): generic args are invariant. Even though `int`
        // widens to `float`, `Array<int>` is **not** assignable to
        // `Array<float>` (the runtime rejects this — we trust the
        // runtime as the oracle). The reverse is also rejected.
        assert!(!is_assignable_to(&a, arr_int, arr_float));
        assert!(!is_assignable_to(&a, arr_float, arr_int));
        assert!(is_assignable_to(&a, arr_int, arr_int));
    }

    #[test]
    fn generic_name_mismatch_stays_unassignable() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let arr_int = a.generic("Array", vec![int]);
        let set_int = a.generic("Set", vec![int]);
        // Different generic names with the same args still mismatch.
        // Inheritance-aware assignability (`type Child<T> extends
        // Parent<T>`) is a later phase.
        assert!(!is_assignable_to(&a, arr_int, set_int));
    }

    #[test]
    fn lambda_with_any_slot_is_symmetric() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let any = a.any();
        // After P20.1, `any` is interchangeable with any other type
        // (both top *and* bottom in the lattice — mirrors the runtime
        // which compiles `any → T` and defers the type check). So a
        // lambda with `any` in any slot is mutually assignable with a
        // lambda that has a concrete type in the same slot.
        // `f1: (any) -> int` ↔ `f2: (int) -> any`:
        //   * f1 → f2: param needs `int → any` ✓, return needs `int → any` ✓.
        //   * f2 → f1: param needs `any → int` ✓ (P20.1), return needs `any → int` ✓.
        let f1 = a.lambda(vec![any], int);
        let f2 = a.lambda(vec![int], any);
        assert!(is_assignable_to(&a, f1, f2));
        assert!(is_assignable_to(&a, f2, f1));
    }

    #[test]
    fn lambda_arity_mismatch_rejected() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        // Arity mismatch is hard-rejected regardless of the `any`
        // bidirectionality from P20.1 — no slot count, no relation.
        let f1 = a.lambda(vec![int], int);
        let f2 = a.lambda(vec![int, int], int);
        assert!(!is_assignable_to(&a, f1, f2));
        assert!(!is_assignable_to(&a, f2, f1));
    }

    #[test]
    fn union_member_flows_in() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let str_t = a.primitive(Primitive::String);
        let union = a.alloc(Type {
            kind: TypeKind::Union {
                alts: vec![int, str_t],
            },
            nullable: false,
        });
        assert!(is_assignable_to(&a, int, union));
        assert!(is_assignable_to(&a, str_t, union));
        let bool_t = a.primitive(Primitive::Bool);
        assert!(!is_assignable_to(&a, bool_t, union));
    }

    #[test]
    fn registry_lookup() {
        let mut a = fresh();
        let mut reg = TypeRegistry::new();
        let foo = a.named("Foo");
        reg.register("Foo", foo);
        assert_eq!(reg.lookup("Foo"), Some(foo));
        assert!(reg.lookup("Bar").is_none());
    }

    #[test]
    fn symbol_table_intern_is_idempotent() {
        let mut t = SymbolTable::new();
        let a1 = t.intern("alpha");
        let a2 = t.intern("alpha");
        let b = t.intern("beta");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_eq!(t.resolve(a1), Some("alpha"));
        assert_eq!(t.resolve(b), Some("beta"));
        assert_eq!(t.lookup("alpha"), Some(a1));
        assert!(t.lookup("gamma").is_none());
    }

    #[test]
    fn display_renders_nullable_suffix() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        let str_t = a.primitive(Primitive::String);
        let arr = a.generic("Array", vec![str_t]);
        assert_eq!(a.display(int_q).to_string(), "int?");
        assert_eq!(a.display(arr).to_string(), "Array<String>");
    }

    #[test]
    fn node_tag_auto_derefs_to_inner() {
        let mut a = fresh();
        let person = a.named("Person");
        let node_person = a.generic("node", vec![person]);
        // node<Person> → Person  (auto-deref)
        assert!(is_assignable_to(&a, node_person, person));
        // Person → node<Person>  is NOT auto-promoted.
        assert!(!is_assignable_to(&a, person, node_person));
    }

    #[test]
    fn inference_table_substitutes_generic_params() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let t_param = a.alloc(Type {
            kind: TypeKind::GenericParam {
                name: "T".into(),
                owner: GenericOwner::Type("Foo".into()),
            },
            nullable: false,
        });
        let arr_t = a.generic("Array", vec![t_param]);

        let mut tbl = InferenceTable::new();
        tbl.bind("T", int);

        let resolved = tbl.substitute(&mut a, arr_t);
        let resolved_kind = &a.get(resolved).kind;
        let TypeKind::Generic { name, args } = resolved_kind else {
            panic!("expected Array<int>, got {resolved_kind:?}");
        };
        assert_eq!(name, "Array");
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int]);
    }

    #[test]
    fn arena_substitute_replaces_generic_params() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let str_t = a.primitive(Primitive::String);
        let t_param = a.alloc(Type {
            kind: TypeKind::GenericParam {
                name: "T".into(),
                owner: GenericOwner::Type("Foo".into()),
            },
            nullable: false,
        });
        let u_param = a.alloc(Type {
            kind: TypeKind::GenericParam {
                name: "U".into(),
                owner: GenericOwner::Type("Foo".into()),
            },
            nullable: false,
        });
        let map_tu = a.generic("Map", vec![t_param, u_param]);

        let mut subst: FxHashMap<String, TypeId> = FxHashMap::default();
        subst.insert("T".into(), int);
        subst.insert("U".into(), str_t);

        let resolved = a.substitute(map_tu, &subst);
        let TypeKind::Generic { name, args } = &a.get(resolved).kind else {
            panic!("expected Map<int, String>");
        };
        assert_eq!(name, "Map");
        // P25.7: args is `SmallVec<[TypeId; 2]>` — compare via slices.
        assert_eq!(args.as_slice(), &[int, str_t]);

        // Idempotent: re-applying yields the same TypeId.
        let resolved2 = a.substitute(resolved, &subst);
        assert_eq!(resolved, resolved2);

        // Nullability preserved: Array<T?> with T → int gives Array<int?>.
        let t_param_q = a.nullable(t_param);
        let arr_t_q = a.generic("Array", vec![t_param_q]);
        let resolved_q = a.substitute(arr_t_q, &subst);
        let TypeKind::Generic { args: q_args, .. } = &a.get(resolved_q).kind else {
            panic!();
        };
        assert!(a.get(q_args[0]).nullable);
    }

    #[test]
    fn arena_substitute_no_op_on_empty_subst() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let arr = a.generic("Array", vec![int]);
        let empty: FxHashMap<String, TypeId> = FxHashMap::default();
        assert_eq!(a.substitute(arr, &empty), arr);
    }
}
