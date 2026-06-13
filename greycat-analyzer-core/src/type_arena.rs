//! [`TypeArena`] — the append-only interning pool for [`Type`]s — and
//! [`Builtins`], the canonical [`ItemId`]s for the native-core well-known
//! types the subtyping rules reason about.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{ItemId, Primitive, Symbol, SymbolTable, Type, TypeId, TypeKind};

/// Canonical `ItemId` per well-known native-core type (declared in
/// `lib/std/core.gcl`). A primitive `int` is `Type(ItemId(core, int))`;
/// a node tag `node<T>` is `Generic { tpl: ItemId(core, node), .. }`.
///
/// Std-free: an `ItemId` is two interned symbols, so these identities are
/// valid whether or not the stdlib is loaded.
#[derive(Debug, Clone, Copy)]
pub struct Builtins {
    pub bool_: ItemId,
    pub int: ItemId,
    pub float: ItemId,
    pub char_: ItemId,
    pub string: ItemId,
    pub time: ItemId,
    pub duration: ItemId,
    pub geo: ItemId,
    pub node: ItemId,
    pub node_time: ItemId,
    pub node_index: ItemId,
    pub node_list: ItemId,
    pub node_geo: ItemId,
}

impl Builtins {
    /// Intern the `core` module symbol and each native-type name against
    /// `symbols`, composing the `(core, name)` handles. Idempotent.
    pub fn compute(symbols: &SymbolTable) -> Self {
        let core = symbols.intern("core");
        let mk = |name: &str| ItemId::new(core, symbols.intern(name));
        Self {
            bool_: mk("bool"),
            int: mk("int"),
            float: mk("float"),
            char_: mk("char"),
            string: mk("String"),
            time: mk("time"),
            duration: mk("duration"),
            geo: mk("geo"),
            node: mk("node"),
            node_time: mk("nodeTime"),
            node_index: mk("nodeIndex"),
            node_list: mk("nodeList"),
            node_geo: mk("nodeGeo"),
        }
    }
}

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
    /// Canonical well-known type identities (see [`Builtins`]), set once
    /// via [`Self::set_builtins`] when the project symbol table is known.
    /// `None` on a bare arena (the primitive cast rules then no-op).
    builtins: Option<Builtins>,
}

impl TypeArena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the project's canonical well-known type identities. Called
    /// once the symbol table is known (`ProjectIndex::with_symbols`).
    pub fn set_builtins(&mut self, builtins: Builtins) {
        self.builtins = Some(builtins);
    }

    /// The canonical well-known type identities, or `None` on a bare
    /// arena that never had them set.
    pub fn builtins(&self) -> Option<&Builtins> {
        self.builtins.as_ref()
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

    /// Allocates a [`TypeKind::Any`]
    pub fn any_nullable(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Any,
            nullable: true,
        })
    }

    /// Allocates a [`TypeKind::Never`]
    pub fn never(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Never,
            nullable: false,
        })
    }

    /// Allocates a [`TypeKind::Type`]
    pub fn alloc_type(&mut self, id: ItemId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Type(id),
            nullable: false,
        })
    }

    /// Allocates a [`TypeKind::Generic`].
    /// Caller guarantees `args` is non-empty:
    /// zero-arg uses of a generic decl are an upstream lowering
    /// error, not a value-shaped concept.
    pub fn alloc_generic(&mut self, tpl: ItemId, args: Vec<TypeId>) -> TypeId {
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
    pub fn tuple(&mut self, tpl: ItemId, x: TypeId, y: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Generic {
                tpl,
                args: [x, y].into(),
            },
            nullable: false,
        })
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
    /// `Union` shapes. Non-substitutable kinds (`Type`, `Primitive`,
    /// `Null`, `Any`, `Never`, `Enum`, `Unresolved`) return `ty`
    /// unchanged.
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

    // pub fn is_a(&self, ty: TypeId, target: TypeId) -> bool {
    //     match self.items[ty.0 as usize].kind {
    //         TypeKind::Type(id)
    //     }
    // }

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
            TypeKind::Union { alts } => alts.iter().all(|alt| self.is_assignable_to(*alt, to)),

            TypeKind::Primitive(pa) => match &b.kind {
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by top-level guards")
                }
                TypeKind::Primitive(pb) => primitive_assignable(*pa, *pb),
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

            // Decl identity via `ItemId`. Cross-module references
            // to the same decl share the same `(module, name)` pair.
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
                | TypeKind::Primitive(_)
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
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
                TypeKind::Null
                | TypeKind::Never
                | TypeKind::Primitive(_)
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
                TypeKind::Union { alts } => {
                    alts.iter().any(|alt| self.is_assignable_to(from, *alt))
                }
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
                if is_int_target(to_t) {
                    return true;
                }
                self.is_assignable_to_strip_source_nullable(from, to)
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
                    | TypeKind::TypeOf(_) => self.is_assignable_to_strip_source_nullable(from, to),
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
                    | TypeKind::TypeOf(_) => self.is_assignable_to_strip_source_nullable(from, to),
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
                    | TypeKind::TypeOf(_) => self.is_assignable_to_strip_source_nullable(from, to),
                },
                Primitive::Bool
                | Primitive::String
                | Primitive::Time
                | Primitive::Duration
                | Primitive::Geo => self.is_assignable_to_strip_source_nullable(from, to),
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
            | TypeKind::Lambda { .. } => self.is_assignable_to_strip_source_nullable(from, to),
        }
    }
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
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by guards above")
                }
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
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by guards above")
                }
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
                TypeKind::Any | TypeKind::Unresolved { .. } => {
                    unreachable!("filtered by guards above")
                }
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
}

fn is_int_target(t: &Type) -> bool {
    matches!(t.kind, TypeKind::Primitive(Primitive::Int))
}

fn primitive_assignable(from: Primitive, to: Primitive) -> bool {
    // GreyCat's runtime rejects every primitive-to-primitive
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
