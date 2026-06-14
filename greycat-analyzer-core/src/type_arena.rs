//! [`TypeArena`] — the append-only interning pool for [`Type`]s — and
//! [`Builtins`], the canonical [`ItemKey`]s for the native-core well-known
//! types the subtyping rules reason about.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{ItemKey, Symbol, SymbolTable, Type, TypeId, TypeKind};

/// Canonical `ItemKey` per well-known native-core type (declared in
/// `lib/std/core.gcl`). A primitive `int` is `Type(ItemKey(core, int))`;
/// a node tag `node<T>` is `Generic { tpl: ItemKey(core, node), .. }`.
///
/// Std-free: an `ItemKey` is two interned symbols, so these identities are
/// valid whether or not the stdlib is loaded.
#[derive(Debug, Clone, Copy)]
pub struct Builtins {
    pub bool_: TypeId,
    pub int: TypeId,
    pub float: TypeId,
    pub char_: TypeId,
    pub string: TypeId,
    pub time: TypeId,
    pub duration: TypeId,
    pub geo: TypeId,
    pub any: TypeId,
    pub null: TypeId,
    pub never: TypeId,
    pub node: TypeId,
    pub node_time: TypeId,
    pub node_index: TypeId,
    pub node_list: TypeId,
    pub node_geo: TypeId,
    /// The `core::any` / `core::null` decl keys. Source `any` / `null`
    /// resolve to these decls but lower to the `any` / `null` *variants*
    /// above, which a nominal `Type(core::X)` can't encode.
    pub any_key: ItemKey,
    pub null_key: ItemKey,
}

impl Builtins {
    /// The always-available core type names: the 8 primitives, the 5
    /// `node` tags, and `any` / `null`. Their `core::X` identity holds
    /// with or without `core.gcl` loaded, so `resolve_item` (type
    /// lowering) and `has_name` (the resolver's known-name fallback) both
    /// recognise them in a std-free project.
    const CORE_TYPE_NAMES: [&'static str; 15] = [
        "bool",
        "int",
        "float",
        "char",
        "time",
        "duration",
        "String",
        "geo",
        "node",
        "nodeList",
        "nodeIndex",
        "nodeGeo",
        "nodeTime",
        "any",
        "null",
    ];

    /// `true` iff `name` is one of [`Self::CORE_TYPE_NAMES`].
    pub fn is_core_type_name(name: &str) -> bool {
        Self::CORE_TYPE_NAMES.contains(&name)
    }
}

/// Append-only interning arena for `Type`. Two equal `Type` values get
/// the same [`TypeId`]; comparing for equality is then just an integer
/// comparison.
///
/// The arena does **not** itself store decl names — `TypeKind::Type` /
/// `TypeKind::Generic` carry an [`ItemKey`] `(module_sym, name_sym)`
/// pair. Rendering them to a printable string needs the project's
/// [`SymbolTable`] to resolve the symbols back to text; see
/// `greycat_analyzer_analysis::project::display_type` and
/// `greycat_analyzer_analysis::display_fqn`.
#[derive(Debug, Clone)]
pub struct TypeArena {
    pub items: Vec<Type>,
    pub intern: FxHashMap<Type, TypeId>,
    pub builtins: Builtins,
}

impl TypeArena {
    pub fn new(symbols: &SymbolTable) -> Self {
        let mut items = Vec::with_capacity(128);
        let mut intern = FxHashMap::with_capacity_and_hasher(128, Default::default());

        let any_id = {
            let id = TypeId(items.len() as u32);
            let ty = Type {
                kind: TypeKind::Any,
                nullable: false,
            };
            items.push(ty.clone());
            intern.insert(ty, id);
            id
        };

        let null_id = {
            let id = TypeId(items.len() as u32);
            let ty = Type {
                kind: TypeKind::Null,
                nullable: true,
            };
            items.push(ty.clone());
            intern.insert(ty, id);
            id
        };

        let never_id = {
            let id = TypeId(items.len() as u32);
            let ty = Type {
                kind: TypeKind::Never,
                nullable: false,
            };
            items.push(ty.clone());
            intern.insert(ty, id);
            id
        };

        let core = symbols.intern("core");
        let any_key = ItemKey::new(core, symbols.intern("any"));
        let null_key = ItemKey::new(core, symbols.intern("null"));
        let mut alloc_type = |name: &str| {
            let id = TypeId(items.len() as u32);
            let ty = Type {
                kind: TypeKind::Type(ItemKey::new(core, symbols.intern(name))),
                nullable: false,
            };
            items.push(ty.clone());
            intern.insert(ty, id);
            id
        };
        let builtins = Builtins {
            any: any_id,
            null: null_id,
            never: never_id,
            any_key,
            null_key,
            bool_: alloc_type("bool"),
            int: alloc_type("int"),
            float: alloc_type("float"),
            char_: alloc_type("char"),
            string: alloc_type("String"),
            time: alloc_type("time"),
            duration: alloc_type("duration"),
            geo: alloc_type("geo"),
            node: alloc_type("node"),
            node_time: alloc_type("nodeTime"),
            node_index: alloc_type("nodeIndex"),
            node_list: alloc_type("nodeList"),
            node_geo: alloc_type("nodeGeo"),
        };
        Self {
            items,
            intern,
            builtins,
        }
    }

    /// Allocates a [`Type`] or yield the [`TypeId`] if already interned.
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

    pub fn resolve(&self, ty: &Type) -> Option<TypeId> {
        self.intern.get(ty).copied()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Make a copy of `id` with `nullable = true`. Idempotent.
    pub fn nullable(&mut self, id: TypeId) -> TypeId {
        let ty = self.get(id);
        if ty.nullable {
            return id;
        }
        let mut new_ty = ty.clone();
        new_ty.nullable = true;
        self.alloc(new_ty)
    }

    /// Makes a copy of `id` with `nullable = false`. Idempotent.
    pub fn strip_nullable(&mut self, id: TypeId) -> TypeId {
        let ty = self.get(id);
        if !ty.nullable {
            return id;
        }
        let mut new_ty = ty.clone();
        new_ty.nullable = false;
        self.alloc(new_ty)
    }

    /// Yields the [`TypeId`] of the `null` type.
    pub fn null(&mut self) -> TypeId {
        self.builtins.null
    }

    /// Yields the [`TypeId`] of the `any` type.
    /// This is **not** nullable.
    pub fn any(&mut self) -> TypeId {
        self.builtins.any
    }

    /// Allocates a [`TypeKind::Any`] or yield the [`TypeId`] if already interned.
    pub fn any_nullable(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Any,
            nullable: true,
        })
    }

    /// Yields the [`TypeId`] of the `never` type.
    pub fn never(&mut self) -> TypeId {
        self.builtins.never
    }

    /// Allocates a [`TypeKind::Type`] or yield the [`TypeId`] if already interned.
    pub fn alloc_type(&mut self, id: ItemKey) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Type(id),
            nullable: false,
        })
    }

    /// Allocates a [`TypeKind::Generic`] or yield the [`TypeId`] if already interned.
    /// Caller guarantees `args` is non-empty:
    /// zero-arg uses of a generic decl are an upstream lowering
    /// error, not a value-shaped concept.
    pub fn alloc_generic(&mut self, tpl: ItemKey, args: Vec<TypeId>) -> TypeId {
        debug_assert!(!args.is_empty(), "Generic must have non-empty args");
        self.alloc(Type {
            kind: TypeKind::Generic {
                tpl,
                args: args.into(),
            },
            nullable: false,
        })
    }

    /// Allocates a [`TypeKind::Unresolved`]. Use this in place of the
    /// `self.any()` fallback when a type-ref name didn't resolve —
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

    pub fn generic_param(&mut self, name: Symbol /*, owner: GenericOwner */) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::GenericParam(name),
            nullable: false,
        })
    }

    pub fn lambda(&mut self, params: Vec<TypeId>, ret: Option<TypeId>) -> TypeId {
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
    pub fn tuple(&mut self, tpl: ItemKey, x: TypeId, y: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Generic {
                tpl,
                args: [x, y].into(),
            },
            nullable: false,
        })
    }

    pub fn is_builtin(&self, ty: TypeId) -> bool {
        ty == self.builtins.bool_
            || ty == self.builtins.char_
            || ty == self.builtins.duration
            || ty == self.builtins.float
            || ty == self.builtins.geo
            || ty == self.builtins.int
            || ty == self.builtins.node
            || ty == self.builtins.node_geo
            || ty == self.builtins.node_index
            || ty == self.builtins.node_list
            || ty == self.builtins.node_time
    }

    /// Substitute `TypeParam` occurrences inside `ty`
    /// with the matching entry in `subst`, allocating fresh interned
    /// types for any container that changed shape. Idempotent: calling
    /// twice produces the same TypeId. Mirrors
    /// [`InferenceTable::substitute`] but takes a plain `&FxHashMap` so
    /// callers (e.g. the staged-pipeline body walker) don't have to
    /// route witnesses through an `InferenceTable`.
    ///
    /// Recurses through `Generic`, `Tuple`, `Lambda`, `Anonymous`, and
    /// `Union` shapes. Non-substitutable kinds (`Type`, `Null`, `Any`,
    /// `Never`, `Enum`, `Unresolved`) return `ty` unchanged.
    pub fn substitute(&mut self, ty: TypeId, subst: &FxHashMap<Symbol, TypeId>) -> TypeId {
        if subst.is_empty() {
            return ty;
        }
        let t = self.get(ty).clone();
        match &t.kind {
            TypeKind::GenericParam(name) => match subst.get(name) {
                Some(&witness) if t.nullable => self.nullable(witness),
                Some(&witness) => witness,
                None => ty,
            },
            // `Type(tpl)` is non-generic, no params to substitute.
            TypeKind::Type(_) => ty,
            TypeKind::Generic { tpl, args } => {
                let new_args: SmallVec<[TypeId; 2]> =
                    args.iter().map(|a| self.substitute(*a, subst)).collect();
                if &new_args == args {
                    ty
                } else {
                    self.alloc(Type {
                        kind: TypeKind::Generic {
                            tpl: *tpl,
                            args: new_args,
                        },
                        nullable: t.nullable,
                    })
                }
            }
            TypeKind::Lambda { params, ret } => {
                let new_params: Box<[TypeId]> =
                    params.iter().map(|p| self.substitute(*p, subst)).collect();
                let new_ret = ret.map(|r| self.substitute(r, subst));
                if new_ret == *ret && &new_params == params {
                    ty
                } else {
                    self.alloc(Type {
                        kind: TypeKind::Lambda {
                            params: new_params,
                            ret: new_ret,
                        },
                        nullable: t.nullable,
                    })
                }
            }
            TypeKind::Union { alts } => {
                let new_alts: Box<[TypeId]> =
                    alts.iter().map(|a| self.substitute(*a, subst)).collect();
                if &new_alts == alts {
                    ty
                } else {
                    self.alloc(Type {
                        kind: TypeKind::Union { alts: new_alts },
                        nullable: t.nullable,
                    })
                }
            }
            TypeKind::TypeOf(inner) => {
                let new_inner = self.substitute(*inner, subst);
                if new_inner == *inner {
                    ty
                } else {
                    self.alloc(Type {
                        kind: TypeKind::TypeOf(new_inner),
                        nullable: t.nullable,
                    })
                }
            }
            _ => ty,
        }
    }

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
    pub fn is_assignable_to(&self, from: TypeId, to: TypeId) -> bool {
        if from == to {
            return true;
        }
        let a = self.get(from);
        let b = self.get(to);

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
        // `any` is *also* the bottom type. The GreyCat
        // compiler accepts `any → T` for any `T` (it compiles cleanly
        // and defers the type check to runtime assignment / call time);
        // the static analyzer must match. Source nullability is ignored:
        // `any?` → `T` also passes.
        if matches!(a.kind, TypeKind::Any) {
            return true;
        }
        // `Unresolved` behaves like `any` on either side so a
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
            TypeKind::Union { alts } => alts.iter().all(|alt| self.is_assignable_to(*alt, to)),

            // Decl identity via `ItemKey`. Cross-module references
            // to the same decl share the same `(module, name)` pair.
            // The 8 primitives are `Type(core::X)` decls, so primitive
            // identity (`int == int`, `int != float`) flows through here.
            // Supertype-chain assignability lives in
            // `is_assignable_to_with_index`.
            TypeKind::Type(da) => match &b.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Type(db) => da == db,
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::GenericParam { .. }
                | TypeKind::TypeOf(_) => false,
            },

            // Generic args are invariant
            // (matches the runtime, not the TS checker). The "all-any
            // wildcard" rule is *target-only* and asymmetric: `Foo<any?,
            // any?>` as a TARGET accepts any same-decl instantiation
            // (raw-form acceptance), but as a SOURCE does NOT flow into a
            // concrete `Foo<int, T>` — the runtime rejects with
            // `argument of type 'Foo' is not assignable to parameter of
            // type 'Foo<int, T>'`. Node-tag bivariance lives in
            // `is_assignable_to_with_index`.
            //
            // Invariance compares args by TypeId — arena interning
            // collapses structural equality to identity, so two
            // structurally-equal generic args mint the same `TypeId`.
            // A bidirectional `is_assignable_to(x, y) && is_assignable_to(y, x)`
            // fallback would leak P20.1's any-as-bottom rule into the arg
            // position: `is_assignable_to(any?, int)` returns true via the
            // top-level `Any` source guard, so `Tuple<any?, any?>` would
            // falsely flow into `Tuple<int, AbstractType>`.
            TypeKind::Generic { tpl: da, args: aa } => match &b.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Generic { tpl: db, args: ab } => {
                    if da == db
                        && aa.len() == ab.len()
                        && !ab.is_empty()
                        && ab
                            .iter()
                            .all(|y| matches!(self.get(*y).kind, TypeKind::Any))
                    {
                        return true;
                    }
                    da == db && aa.len() == ab.len() && aa.iter().zip(ab).all(|(x, y)| *x == *y)
                }
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
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
                            .all(|(p_a, p_b)| self.is_assignable_to(*p_b, *p_a))
                        && match (aret, bret) {
                            // Both known: covariant return.
                            (Some(a), Some(b)) => self.is_assignable_to(*a, *b),
                            // Source returns, target discards: fine (slot ignores the value).
                            (Some(_), None) => true,
                            // Target wants a return, source produces none: not assignable.
                            (None, Some(_)) => false,
                            // Neither side observes a return: identity.
                            (None, None) => true,
                        }
                }
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
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
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::GenericParam { .. }
                | TypeKind::TypeOf(_) => false,
            },

            // A generic param `T` (inside a `fn<T>(...)` body) is
            // an opaque type; without an `InferenceTable` witness it
            // doesn't assign to anything concrete except via the top-
            // level `Any`/`Unresolved` guards. Identity is handled by
            // the `from == to` early-return at the top of the function.
            // Target Union still gets the per-alt `any()` retry.
            TypeKind::GenericParam { .. } => match &b.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
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
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::GenericParam { .. } => false,
            },
        }
    }

    /// `true` iff `from` can be casted to `to` via the GreyCat `as` operator.
    pub fn is_castable(&self, from: TypeId, to: TypeId) -> bool {
        // trivial cast to itself is valid
        if from == to {
            return true;
        }

        let from_t = self.get(from);
        let to_t = self.get(to);

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

        // Primitive widening casts: `int<->float`, `char as {String,int}`.
        // Every primitive is a `Type(core::X)` decl, so these are the only
        // same-arena cast relaxations beyond identity / inheritance.
        let int_to_int =
            |from: TypeId, to: TypeId| from == self.builtins.int && to == self.builtins.int;
        let float_to_float =
            |from: TypeId, to: TypeId| from == self.builtins.float && to == self.builtins.float;
        let char_to_string_or_int = |from: TypeId, to: TypeId| {
            from == self.builtins.char_ && (to == self.builtins.string || to == self.builtins.int)
        };
        if int_to_int(from, to) || float_to_float(from, to) || char_to_string_or_int(from, to) {
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
            TypeKind::Any | TypeKind::Unresolved { .. } => {
                unreachable!("filtered by top-level guards")
            }

            // `T as Foo` (where `T` is a generic param)
            // is allowed: the runtime decides at instantiation time.
            TypeKind::GenericParam { .. } => true,

            // P-typeof — type-literal value. The runtime treats `as` as
            // dropped (per the `runtime drops as casts entirely` rule),
            // so cast strictness mirrors assignability: identity through
            // the `from == to` short-circuit at the top of
            // `is_assignable_to`, plus the assignability fall-back below.
            TypeKind::TypeOf(_) => self.is_assignable_to_strip_source_nullable(from, to),

            // Union source: cast iff ANY alt is castable to target.
            // `as` is a runtime-checked downcast — `(A | B) as A` is
            // accepted because the value MIGHT be `A`; if it turns out to
            // be `B` at runtime, the cast panics, which is the documented
            // behavior of `as`. Requiring `.all()` instead would reject
            // the canonical narrow-back-after-?? pattern (kopr's
            // `var x = lhs.get() ?? rhs.get(); ... x as node<L>`).
            // Assignability uses `.all()` for the same shape because
            // assignment is total — no runtime check stands behind it.
            TypeKind::Union { alts } => alts.iter().any(|alt| self.is_castable(*alt, to)),

            // Enum source: castable to `int` (runtime representation) or
            // anything assignable from the same enum.
            TypeKind::Enum { .. } => {
                if to == self.builtins.int {
                    return true;
                }
                self.is_assignable_to_strip_source_nullable(from, to)
            }

            // Everything else (Null, Never, Type, Generic, Lambda) defers
            // to the assignability fall-back. Primitives are `Type(core::X)`
            // and reach here too: their `int<->float` / `char as ..` widening
            // was already accepted by the top-level block above. Node-tag
            // bivariance / `<node-tag> as int` rules live in
            // `is_castable_with_index`. `TypeOf` is handled by its own arm.
            TypeKind::Null
            | TypeKind::Never
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. } => self.is_assignable_to_strip_source_nullable(from, to),
        }
    }

    /// Used by `is_castable`'s fall-back: a cast is
    /// permitted to coerce `T?` to a non-nullable target — the runtime
    /// decides at execution time whether the actual value can land there.
    ///
    /// When the source isn't nullable, delegates straight to
    /// `is_assignable_to`. When it is, we re-do the cheap kind-based
    /// dispatch inline (the arena is `&`, not `&mut`, so we can't intern a
    /// stripped clone and recurse). The inline match is **exhaustive** for
    /// the same reason as `is_assignable_to`: a `_ => false` would silently
    /// absorb future variants.
    fn is_assignable_to_strip_source_nullable(&self, from: TypeId, to: TypeId) -> bool {
        let from_t = self.get(from);
        if !from_t.nullable {
            return self.is_assignable_to(from, to);
        }
        // Top-level guards mirror `is_assignable_to`'s — minus the
        // `a.nullable && !b.nullable` bail we're explicitly trying to skip.
        let to_t = self.get(to);
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
        // Exhaustive nested match. Same-head identity shapes (Type --
        // which includes the 8 primitives -- and Enum) are accepted;
        // everything else rejects.
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
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by guards above")
                }
                TypeKind::Type(db) => da == db,
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::GenericParam { .. }
                | TypeKind::Union { .. }
                | TypeKind::TypeOf(_) => false,
            },
            TypeKind::Enum { name: na, .. } => match &to_t.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by guards above")
                }
                TypeKind::Enum { name: nb, .. } => na == nb,
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
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
}
