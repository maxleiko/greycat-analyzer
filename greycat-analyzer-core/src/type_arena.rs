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
}
