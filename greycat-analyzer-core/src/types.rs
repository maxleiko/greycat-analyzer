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
/// (`foo::Load` and `bar::Load`) coexist unambiguously. Two `ItemId`s
/// compare equal iff they refer to the same item in the same module —
/// one register-sized compare, since both fields are `Copy` u32
/// newtypes under the hood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId {
    pub module: Symbol,
    pub name: Symbol,
}

impl ItemId {
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
    /// `any` — top type. Anything non-nullable is assignable to it.
    Any,
    /// `never` — bottom type. Used for unreachable code.
    Never,
    /// Named primitive — `int`, `float`, `String`, `bool`, `char`,
    /// `time`, `duration`, `geo`. Carries the canonical name.
    Primitive(Primitive),
    // P35.2
    /// A resolved non-generic type — user-defined `type Foo {...}` or
    /// a non-generic native type from `std/core`. The decl's [`ItemId`]
    /// `(module, name)` is the identity; cross-module references to
    /// the same decl share the same `ItemId`, so equality is two
    /// register-sized symbol compares.
    ///
    /// Distinct from [`TypeKind::Generic`] with empty args.
    /// Non-generic types and zero-arg instantiations are different
    /// concepts — separating them by variant lets the substitution /
    /// variance / node-tag-dispatch machinery match only the latter
    /// without runtime `args.is_empty()` checks.
    Type(ItemId),
    // P35.2
    /// An instantiation of a generic decl — `Array<int>`, `node<int?>`,
    /// `Map<String, V>`. `decl` is the generic template's [`ItemId`];
    /// `args` are the per-use-site type arguments and are guaranteed
    /// non-empty by the lowering pass (zero-arg uses of a generic
    /// decl are an analysis error caught upstream).
    Generic {
        decl: ItemId,
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
/// The arena does **not** itself store decl names — `TypeKind::Type` /
/// `TypeKind::Generic` carry an [`ItemId`] `(module_sym, name_sym)`
/// pair. Rendering them to a printable string needs the project's
/// [`SymbolTable`] to resolve the symbols back to text; see
/// `greycat_analyzer_analysis::project::display_type` and
/// `greycat_analyzer_analysis::display_fqn`.
#[derive(Debug, Default, Clone)]
pub struct TypeArena {
    pub items: Vec<Type>,
    pub intern: FxHashMap<Type, TypeId>,
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
    /// Allocate a resolved non-generic [`TypeKind::Type`].
    pub fn alloc_type(&mut self, decl: ItemId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Type(decl),
            nullable: false,
        })
    }

    // P35.2
    /// Allocate a [`TypeKind::Generic`] (decl-keyed generic
    /// instantiation). Caller guarantees `args` is non-empty —
    /// zero-arg uses of a generic decl are an upstream lowering
    /// error, not a value-shaped concept.
    pub fn generic(&mut self, decl: ItemId, args: Vec<TypeId>) -> TypeId {
        debug_assert!(!args.is_empty(), "Generic must have non-empty args");
        self.alloc(Type {
            kind: TypeKind::Generic {
                decl,
                args: args.into(),
            },
            nullable: false,
        })
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

    /// Allocate a [`TypeKind::TypeOf`] wrapping `inner`. Idempotent
    /// (interns through `alloc`). Idiomatic for both the lowering of a
    /// `typeof T` source-form annotation and the expression-typing of a
    /// bare type-ident in value position (e.g. `DurationUnit` passed as
    /// an argument).
    pub fn type_of(&mut self, inner: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::TypeOf(inner),
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
    pub fn tuple(&mut self, decl: ItemId, x: TypeId, y: TypeId) -> TypeId {
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
            TypeKind::TypeOf(inner) => {
                let new_inner = self.substitute(*inner, subst);
                if new_inner == *inner {
                    ty
                } else {
                    let mut new_t = self.type_of(new_inner);
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
/// The body is structured as an **exhaustive** match on `&a.kind` with
/// each arm an **exhaustive** sub-match on `&b.kind`. A wildcard
/// `_ => false` would be more compact but would silently absorb any
/// future `TypeKind` variant (the bug pattern that produced the
/// `Union → supertype` false negative — see analysis-crate's
/// `is_assignable_to_with_index` git history). Adding a new variant
/// now breaks the build in every relevant arm, forcing a conscious
/// decision about how that shape relates to every other shape.
///
/// Inheritance-aware extension (cross-module supertype chains, node-tag
/// bivariance) lives one layer up in
/// `greycat_analyzer_analysis::project::is_assignable_to_with_index`.
pub fn is_assignable_to(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    if from == to {
        return true;
    }
    let a = arena.get(from);
    let b = arena.get(to);

    // Top-level guards. Run before the kind-pair match so the match
    // doesn't have to repeat `Null | Any | Never | Unresolved` rules
    // in every source / target arm. After these, the match can
    // assume: source ≠ Any|Never|Null|Unresolved, target ≠ Any|Unresolved.
    // (Target Null / target Never can still reach the match — they're
    // legitimate "from doesn't fit there" cases handled per source-kind.)

    // Null source: `null` flows into anything nullable.
    if matches!(a.kind, TypeKind::Null) {
        return b.nullable;
    }
    // Never source: bottom type, flows everywhere.
    if matches!(a.kind, TypeKind::Never) {
        return true;
    }
    // Any target: top type, absorbs everything.
    if matches!(b.kind, TypeKind::Any) {
        return true;
    }
    // **P20.1** — `any` is *also* the bottom type. The GreyCat
    // compiler accepts `any → T` for any `T` (it compiles cleanly
    // and defers the type check to runtime assignment / call time);
    // the static analyzer must match. Source nullability is ignored:
    // `any?` → `T` also passes.
    if matches!(a.kind, TypeKind::Any) {
        return true;
    }
    // P35.3 — `Unresolved` behaves like `any` on either side so a
    // single unresolved name doesn't fan out into a cascade of
    // false-positive type-relation diagnostics.
    if matches!(a.kind, TypeKind::Unresolved { .. })
        || matches!(b.kind, TypeKind::Unresolved { .. })
    {
        return true;
    }
    // A non-nullable target rejects a nullable source: `T → T?` is
    // fine, `T? → T` is not.
    if a.nullable && !b.nullable {
        return false;
    }

    // P7.3 (REMOVED): there is no `node<T> → T` auto-deref subtype
    // rule. The runtime rejects `var x: T = some_node<T>();` — the
    // arrow operator (`*n` / `n->m()`) is the *syntactic* desugar for
    // `n.resolve().m()`, dispatched by the `@deref("resolve")`
    // annotation on the receiver's type decl.

    // Exhaustive nested match. Source-kind outer, target-kind inner.
    // The `Any | Unresolved` target arm and the `Null | Any | Never |
    // Unresolved` source arms are `unreachable!()` — caught by the
    // guards above. A future TypeKind variant breaks every outer arm
    // (forcing a source-side decision) AND every inner arm (forcing
    // a target-side decision per existing source). Cross-kind
    // rejections are spelled out explicitly per source arm.
    match &a.kind {
        TypeKind::Null | TypeKind::Any | TypeKind::Never | TypeKind::Unresolved { .. } => {
            unreachable!("filtered by top-level guards")
        }

        // Union source: every alt must assign to the target. Target
        // can itself be a Union — recursive `is_assignable_to` re-
        // enters the (non-Union-source, Union-target) arm below for
        // each alt, which uses `any()`.
        TypeKind::Union { alts } => alts.iter().all(|alt| is_assignable_to(arena, *alt, to)),

        TypeKind::Primitive(pa) => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Primitive(pb) => primitive_assignable(*pa, *pb),
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // P35.2 — decl identity via `ItemId`. Cross-module references
        // to the same decl share the same `(module, name)` pair.
        // Supertype-chain assignability lives in
        // `is_assignable_to_with_index`.
        TypeKind::Type(da) => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Type(db) => da == db,
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // P12.2 / P19.10 / P19.14 — generic args are invariant
        // (matches the runtime, not the TS checker), checked by
        // bidirectional `is_assignable_to` so cross-arena lowering
        // variations still match. An "all-any wildcard" target
        // (`Foo<any, any>`) accepts any same-decl instantiation —
        // mirrors the runtime's raw-form acceptance. Node-tag
        // bivariance lives in `is_assignable_to_with_index`.
        TypeKind::Generic { decl: da, args: aa } => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Generic { decl: db, args: ab } => {
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
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Lambda: contravariant in params, covariant in return.
        TypeKind::Lambda {
            params: aparams,
            ret: aret,
        } => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Lambda {
                params: bparams,
                ret: bret,
            } => {
                aparams.len() == bparams.len()
                    && aparams
                        .iter()
                        .zip(bparams.as_ref())
                        .all(|(p_a, p_b)| is_assignable_to(arena, *p_b, *p_a))
                    && is_assignable_to(arena, *aret, *bret)
            }
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        TypeKind::Enum { name: na, .. } => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Enum { name: nb, .. } => na == nb,
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // P25.4 — a generic param `T` (inside a `fn<T>(...)` body) is
        // an opaque type; without an `InferenceTable` witness it
        // doesn't assign to anything concrete except via the top-
        // level `Any`/`Unresolved` guards. Identity is handled by
        // the `from == to` early-return at the top of the function.
        // Target Union still gets the per-alt `any()` retry.
        TypeKind::GenericParam { .. } => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // P-typeof — `TypeOf(X)` is a *type-literal value*, modelled
        // as a distinct kind from its inner. Identity is by inner-
        // TypeId; equality short-circuits via the `from == to`
        // top-of-function check. Cross-kind targets reject. The
        // analyzer-side `is_assignable_to_with_index` adds the
        // `TypeOf(X) → Type(core::type)` widening so stdlib functions
        // typed `(t: type)` still accept type-literal arguments.
        TypeKind::TypeOf(_) => match &b.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }
            TypeKind::TypeOf(_) => false, // identity is the `from == to` early-return above
            TypeKind::Union { alts } => alts.iter().any(|alt| is_assignable_to(arena, from, *alt)),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. } => false,
        },
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
            TypeKind::TypeOf(inner) => {
                let new_inner = self.substitute(arena, *inner);
                if new_inner == *inner {
                    ty
                } else {
                    let mut new_t = arena.type_of(new_inner);
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

    // Top-level guards: same shape as `is_assignable_to`'s guards
    // (top/bottom type absorption, unresolved-as-any) plus two
    // cast-specific rules: a `GenericParam` target always passes
    // (runtime decides at instantiation time, P19.14), and `Any`
    // target absorbs only non-null sources (`null as any` rejects).
    if matches!(to_t.kind, TypeKind::Any) && !from_t.nullable {
        return true;
    }
    if matches!(to_t.kind, TypeKind::GenericParam { .. }) {
        return true;
    }
    if matches!(to_t.kind, TypeKind::Unresolved { .. } | TypeKind::Any)
        || matches!(from_t.kind, TypeKind::Unresolved { .. } | TypeKind::Any)
    {
        return true;
    }

    // Exhaustive nested match. Same rationale as `is_assignable_to`:
    // a `_ =>` fall-through would silently absorb future TypeKind
    // variants. Cast-specific rules layered on top of an
    // assignability fall-back (`is_assignable_to_strip_source_nullable`)
    // for same-head identity / primitive widening shapes. The
    // fall-back fires per source-kind where no cast-specific rule
    // applies — spelled out explicitly per arm.
    match &from_t.kind {
        TypeKind::Any | TypeKind::Unresolved { .. } => unreachable!("filtered by top-level guards"),

        // **P19.14** — `T as Foo` (where `T` is a generic param)
        // is allowed: the runtime decides at instantiation time.
        TypeKind::GenericParam { .. } => true,

        // P-typeof — type-literal value. The runtime treats `as` as
        // dropped (per the `runtime drops as casts entirely` rule),
        // so cast strictness mirrors assignability: identity through
        // the `from == to` short-circuit at the top of
        // `is_assignable_to`, plus the assignability fall-back below.
        TypeKind::TypeOf(_) => is_assignable_to_strip_source_nullable(arena, from, to),

        // Union source: cast iff ANY alt is castable to target.
        // `as` is a runtime-checked downcast — `(A | B) as A` is
        // accepted because the value MIGHT be `A`; if it turns out to
        // be `B` at runtime, the cast panics, which is the documented
        // behavior of `as`. Requiring `.all()` instead would reject
        // the canonical narrow-back-after-?? pattern (kopr's
        // `var x = lhs.get() ?? rhs.get(); ... x as node<L>`).
        // Assignability uses `.all()` for the same shape because
        // assignment is total — no runtime check stands behind it.
        TypeKind::Union { alts } => alts.iter().any(|alt| is_castable(arena, *alt, to)),

        // Enum source: castable to `int` (runtime representation) or
        // anything assignable from the same enum.
        TypeKind::Enum { .. } => {
            if is_int_target(to_t) {
                return true;
            }
            is_assignable_to_strip_source_nullable(arena, from, to)
        }

        // Primitive source: cast-specific widening rules layered on
        // top of `int as <node-tag>` (handled in
        // `is_castable_with_index`), then assignability fall-back.
        TypeKind::Primitive(p) => match p {
            Primitive::Int => match &to_t.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } | TypeKind::GenericParam { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Primitive(Primitive::Float) => true,
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Primitive(_)
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::Union { .. }
                | TypeKind::TypeOf(_) => is_assignable_to_strip_source_nullable(arena, from, to),
            },
            Primitive::Float => match &to_t.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } | TypeKind::GenericParam { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Primitive(Primitive::Int) => true,
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Primitive(_)
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::Union { .. }
                | TypeKind::TypeOf(_) => is_assignable_to_strip_source_nullable(arena, from, to),
            },
            Primitive::Char => match &to_t.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } | TypeKind::GenericParam { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Primitive(Primitive::String | Primitive::Int) => true,
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Primitive(_)
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::Union { .. }
                | TypeKind::TypeOf(_) => is_assignable_to_strip_source_nullable(arena, from, to),
            },
            Primitive::Bool
            | Primitive::String
            | Primitive::Time
            | Primitive::Duration
            | Primitive::Geo => is_assignable_to_strip_source_nullable(arena, from, to),
        },

        // Everything else (Null source, Never source, Type, Generic,
        // Lambda) defers to the assignability fall-back. Node-tag
        // bivariance / `<node-tag> as int` rules live in
        // `is_castable_with_index`. `TypeOf` is handled by its own
        // arm above.
        TypeKind::Null
        | TypeKind::Never
        | TypeKind::Type(_)
        | TypeKind::Generic { .. }
        | TypeKind::Lambda { .. } => is_assignable_to_strip_source_nullable(arena, from, to),
    }
}

/// Same as `is_assignable_to` but treats the source as if its nullable
/// flag were stripped. Used by `is_castable`'s fall-back: a cast is
/// permitted to coerce `T?` to a non-nullable target — the runtime
/// decides at execution time whether the actual value can land there.
///
/// When the source isn't nullable, delegates straight to
/// `is_assignable_to`. When it is, we re-do the cheap kind-based
/// dispatch inline (the arena is `&`, not `&mut`, so we can't intern a
/// stripped clone and recurse). The inline match is **exhaustive** for
/// the same reason as `is_assignable_to`: a `_ => false` would silently
/// absorb future variants.
fn is_assignable_to_strip_source_nullable(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    let from_t = arena.get(from);
    if !from_t.nullable {
        return is_assignable_to(arena, from, to);
    }
    // Top-level guards mirror `is_assignable_to`'s — minus the
    // `a.nullable && !b.nullable` bail we're explicitly trying to skip.
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
    if matches!(from_t.kind, TypeKind::Any) {
        return true;
    }
    if matches!(from_t.kind, TypeKind::Unresolved { .. })
        || matches!(to_t.kind, TypeKind::Unresolved { .. })
    {
        return true;
    }
    // Exhaustive nested match. Same-head identity shapes (Type, Enum)
    // and primitive widening are accepted; everything else rejects.
    // Generic / Lambda / Union / GenericParam fall to `false` here —
    // they're rare in the `as`-position fallthrough and would need
    // their own cast-side variance / structural rules to handle
    // correctly. (If we ever lift those, this match is the one place
    // to teach.)
    match &from_t.kind {
        TypeKind::Null | TypeKind::Any | TypeKind::Never | TypeKind::Unresolved { .. } => {
            unreachable!("filtered by guards above")
        }

        TypeKind::Type(da) => match &to_t.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => unreachable!("filtered by guards above"),
            TypeKind::Type(db) => da == db,
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::Union { .. }
            | TypeKind::TypeOf(_) => false,
        },
        TypeKind::Enum { name: na, .. } => match &to_t.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => unreachable!("filtered by guards above"),
            TypeKind::Enum { name: nb, .. } => na == nb,
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::Union { .. }
            | TypeKind::TypeOf(_) => false,
        },
        TypeKind::Primitive(pa) => match &to_t.kind {
            TypeKind::Any | TypeKind::Unresolved { .. } => unreachable!("filtered by guards above"),
            TypeKind::Primitive(pb) => primitive_assignable(*pa, *pb),
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::Union { .. }
            | TypeKind::TypeOf(_) => false,
        },
        // P-typeof — source nullability stripped. `TypeOf(X) → TypeOf(Y)`
        // is identity through `from == to`; nothing else accepts.
        TypeKind::TypeOf(_) => false,
        TypeKind::Generic { .. }
        | TypeKind::Lambda { .. }
        | TypeKind::GenericParam { .. }
        | TypeKind::Union { .. } => false,
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

// Type display (decl-name aware) lives in the analysis crate —
// see `greycat_analyzer_analysis::project::display_type` for the
// `TypeWithDecls` wrapper, `greycat_analyzer_analysis::display_fqn`
// for fully-qualified rendering. Core does not own a `SymbolTable`
// and therefore cannot resolve `ItemId` halves back to source text.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SymbolTable;

    #[derive(Default)]
    struct TextCx {
        arena: TypeArena,
        symbols: SymbolTable,
    }

    impl TextCx {
        /// Mint a synthetic [`ItemId`] for tests — every test module
        /// shares the same fake module symbol `"test_mod"`, so two
        /// items with the same `name` collapse to the same identity.
        fn item(&self, name: &str) -> ItemId {
            ItemId::new(self.symbols.intern("test_mod"), self.symbols.intern(name))
        }
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
        let array_decl = cx.item("Array");
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
        let array_decl = cx.item("Array");
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
        let array_decl = cx.item("Array");
        let set_decl = cx.item("Set");
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
        let array_decl = cx.item("Array");
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
        let array_decl = cx.item("Array");
        let map_decl = cx.item("Map");
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
        let array_decl = cx.item("Array");
        let arr = cx.arena.generic(array_decl, vec![int]);
        let empty: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        assert_eq!(cx.arena.substitute(arr, &empty), arr);
    }
}
