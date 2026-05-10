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
    /// Named user / stdlib type, identified by its fully-qualified name
    /// (`<lib>::<module>::<TypeName>` or just `<TypeName>` until we wire
    /// fully-qualified resolution).
    // P25.4
    Named { name: SmolStr },
    /// Generic type instantiation — `Array<int>`, `Map<String, int>`, etc.
    // P25.4 / P25.7
    Generic {
        name: SmolStr,
        args: SmallVec<[TypeId; 2]>,
    },
    /// Generic type *parameter* — the `T` inside a `fn<T>(x: T)` body.
    // P25.4
    GenericParam { name: SmolStr, owner: GenericOwner },
    /// Function / lambda type.
    Lambda(LambdaType),
    /// Tuple — `t2`, `t3`, `t4` plus their float variants.
    Tuple { elements: Vec<TypeId> },
    /// Anonymous object literal type — `{ a: int, b: String }`.
    // P25.4
    Anonymous { fields: Vec<(SmolStr, TypeId)> },
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
#[derive(Debug, Default, Clone)]
pub struct TypeArena {
    items: Vec<Type>,
    intern: FxHashMap<Type, TypeId>,
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

    // P28.1
    /// Read-only intern lookup. Returns the existing [`TypeId`] for
    /// `ty` or `None` if no equal `Type` has been allocated yet.
    /// Used by [`LocalArena`] to dedup against the canonical arena's
    /// snapshot without taking a mutable borrow.
    pub fn lookup(&self, ty: &Type) -> Option<TypeId> {
        self.intern.get(ty).copied()
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

    pub fn any(&mut self) -> TypeId {
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

    pub fn tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Tuple { elements },
            nullable: false,
        })
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
            TypeKind::Tuple { elements } => {
                let new_els: Vec<TypeId> = elements
                    .iter()
                    .map(|e| self.substitute(*e, subst))
                    .collect();
                if new_els == *elements {
                    ty
                } else {
                    let mut new_t = self.tuple(new_els);
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
            TypeKind::Anonymous { fields } => {
                let new_fields: Vec<(SmolStr, TypeId)> = fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.substitute(*t, subst)))
                    .collect();
                if new_fields == *fields {
                    ty
                } else {
                    let mut new_t = self.alloc(Type {
                        kind: TypeKind::Anonymous { fields: new_fields },
                        nullable: false,
                    });
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
// Read-only arena view + LocalArena (P28.1)
// =============================================================================

// P28.1
/// Read-only view of a typed arena. Both [`TypeArena`] and
/// [`LocalArena`] implement this so the generic accessor functions
/// ([`is_assignable_to`], [`is_castable`], [`display`],
/// [`display_fqn`]) work uniformly across the canonical project arena
/// and per-thread local arenas built during the parallel S12 body
/// walk.
pub trait TypeView {
    fn get(&self, id: TypeId) -> &Type;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TypeView for TypeArena {
    fn get(&self, id: TypeId) -> &Type {
        TypeArena::get(self, id)
    }
    fn len(&self) -> usize {
        TypeArena::len(self)
    }
}

// P28.1
/// Append-side trait for typed arenas. Lets [`InferenceTable::substitute`]
/// and any other mutator that only needs to alloc + nullable work
/// uniformly over [`TypeArena`] and [`LocalArena`].
pub trait TypeArenaMut: TypeView {
    fn alloc(&mut self, ty: Type) -> TypeId;
    fn nullable(&mut self, id: TypeId) -> TypeId;
}

impl TypeArenaMut for TypeArena {
    fn alloc(&mut self, ty: Type) -> TypeId {
        TypeArena::alloc(self, ty)
    }
    fn nullable(&mut self, id: TypeId) -> TypeId {
        TypeArena::nullable(self, id)
    }
}

impl TypeArenaMut for LocalArena<'_> {
    fn alloc(&mut self, ty: Type) -> TypeId {
        LocalArena::alloc(self, ty)
    }
    fn nullable(&mut self, id: TypeId) -> TypeId {
        LocalArena::nullable(self, id)
    }
}

// P28.1
/// Per-thread interning arena that layers a private append-only tail
/// on top of a read-only snapshot of the canonical [`TypeArena`].
///
/// Workers in the parallel S12 body walker each own a `LocalArena`
/// constructed via [`LocalArena::wrap`]. Allocations dedup first
/// against the canonical snapshot's intern table — so existing
/// canonical [`TypeId`]s are returned unchanged — and then against
/// the local tail. New mints land in the tail, with TypeIds offset
/// past the snapshot's length.
///
/// After the parallel phase the orchestrator calls
/// [`TypeArena::merge_local`] on each thread's
/// [`LocalArena::into_local_items`], producing a remap table that
/// canonicalizes every locally-minted [`TypeId`] back into the
/// project-wide arena. The remap is then applied to every
/// `TypeId`-carrying field of each per-module result.
pub struct LocalArena<'a> {
    base: &'a TypeArena,
    base_len: u32,
    local_items: Vec<Type>,
    local_intern: FxHashMap<Type, TypeId>,
}

impl<'a> LocalArena<'a> {
    /// Wrap a snapshot of `base`. The wrapped [`TypeArena`] is treated
    /// as read-only for the lifetime of this `LocalArena`; new mints
    /// land in a private tail. `base.len()` is captured at construction
    /// — the merge step uses the captured value to distinguish base
    /// TypeIds (`raw() < base_len`) from local-tail TypeIds.
    pub fn wrap(base: &'a TypeArena) -> Self {
        let base_len = base.len() as u32;
        Self {
            base,
            base_len,
            local_items: Vec::new(),
            local_intern: FxHashMap::default(),
        }
    }

    /// Snapshot length captured at construction. Local TypeIds have
    /// `raw() >= base_len`; canonical TypeIds have `raw() < base_len`.
    pub fn base_len(&self) -> u32 {
        self.base_len
    }

    /// Number of types minted into the local tail.
    pub fn local_len(&self) -> usize {
        self.local_items.len()
    }

    /// Range of TypeIds minted locally — `base_len..(base_len +
    /// local_len)`. Useful for the merge step's remap walk.
    pub fn new_local_ids(&self) -> std::ops::Range<u32> {
        self.base_len..(self.base_len + self.local_items.len() as u32)
    }

    /// Consume self, returning the locally-minted items in insertion
    /// order. The merge step walks this vec, remaps inner TypeIds
    /// through the partial remap built so far, and allocs into the
    /// canonical arena.
    pub fn into_local_items(self) -> Vec<Type> {
        self.local_items
    }

    pub fn alloc(&mut self, ty: Type) -> TypeId {
        if let Some(id) = self.base.lookup(&ty) {
            return id;
        }
        if let Some(&id) = self.local_intern.get(&ty) {
            return id;
        }
        let id = TypeId(self.base_len + self.local_items.len() as u32);
        self.local_items.push(ty.clone());
        self.local_intern.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &Type {
        let raw = id.0;
        if raw < self.base_len {
            self.base.get(id)
        } else {
            &self.local_items[(raw - self.base_len) as usize]
        }
    }

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

    pub fn any(&mut self) -> TypeId {
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
                args: args.into(),
            },
            nullable: false,
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

    pub fn tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Tuple { elements },
            nullable: false,
        })
    }

    /// Substitute `GenericParam(name)` occurrences inside `ty` with
    /// the matching entry in `subst`. Mirrors [`TypeArena::substitute`]
    /// exactly, but mutates the local tail rather than the canonical
    /// arena.
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
            TypeKind::Tuple { elements } => {
                let new_els: Vec<TypeId> = elements
                    .iter()
                    .map(|e| self.substitute(*e, subst))
                    .collect();
                if new_els == *elements {
                    ty
                } else {
                    let mut new_t = self.tuple(new_els);
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
            TypeKind::Anonymous { fields } => {
                let new_fields: Vec<(SmolStr, TypeId)> = fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.substitute(*t, subst)))
                    .collect();
                if new_fields == *fields {
                    ty
                } else {
                    let mut new_t = self.alloc(Type {
                        kind: TypeKind::Anonymous { fields: new_fields },
                        nullable: false,
                    });
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

impl TypeView for LocalArena<'_> {
    fn get(&self, id: TypeId) -> &Type {
        LocalArena::get(self, id)
    }
    fn len(&self) -> usize {
        (self.base_len + self.local_items.len() as u32) as usize
    }
}

// P28.1
/// Remap every [`TypeId`] inside `ty` through `remap_id`, returning a
/// fresh `Type`. Container-shape kinds (Generic / Lambda / Tuple /
/// Anonymous / Union) are rebuilt with remapped inner ids; leaf kinds
/// (Null / Any / Never / Primitive / Named / GenericParam / Enum) are
/// cloned unchanged.
fn remap_inner_ids<F: Fn(TypeId) -> TypeId>(ty: &Type, remap_id: &F) -> Type {
    let kind = match &ty.kind {
        TypeKind::Null => TypeKind::Null,
        TypeKind::Any => TypeKind::Any,
        TypeKind::Never => TypeKind::Never,
        TypeKind::Primitive(p) => TypeKind::Primitive(*p),
        TypeKind::Named { name } => TypeKind::Named { name: name.clone() },
        TypeKind::GenericParam { name, owner } => TypeKind::GenericParam {
            name: name.clone(),
            owner: owner.clone(),
        },
        TypeKind::Enum { name, variants } => TypeKind::Enum {
            name: name.clone(),
            variants: variants.clone(),
        },
        TypeKind::Generic { name, args } => TypeKind::Generic {
            name: name.clone(),
            args: args.iter().map(|a| remap_id(*a)).collect(),
        },
        TypeKind::Lambda(l) => TypeKind::Lambda(LambdaType {
            params: l.params.iter().map(|p| remap_id(*p)).collect(),
            ret: remap_id(l.ret),
        }),
        TypeKind::Tuple { elements } => TypeKind::Tuple {
            elements: elements.iter().map(|e| remap_id(*e)).collect(),
        },
        TypeKind::Anonymous { fields } => TypeKind::Anonymous {
            fields: fields
                .iter()
                .map(|(n, t)| (n.clone(), remap_id(*t)))
                .collect(),
        },
        TypeKind::Union { alts } => TypeKind::Union {
            alts: alts.iter().map(|a| remap_id(*a)).collect(),
        },
    };
    Type {
        kind,
        nullable: ty.nullable,
    }
}

impl TypeArena {
    // P28.1
    /// Merge a [`LocalArena`]'s locally-minted items into this arena,
    /// returning a remap table indexed by local-tail offset (`0..local_items.len()`).
    /// `remap[i]` is the canonical [`TypeId`] for what was at
    /// `base_len + i` in the LocalArena.
    ///
    /// The merge walks `local_items` in insertion order. Because the
    /// analyzer only refers to TypeIds it has already alloced, every
    /// inner reference points either into the canonical base (TypeId
    /// `< base_len`, passes through unchanged) or into an earlier
    /// position in the local tail (already in `remap`). So the walk
    /// proceeds in one pass — no topological sort needed.
    ///
    /// Callers must apply `remap` to every TypeId-carrying field of
    /// the per-module result that was produced against the LocalArena
    /// (e.g. `AnalysisResult::expr_types`, `def_types`, `registry`).
    pub fn merge_local(&mut self, base_len: u32, local_items: Vec<Type>) -> Vec<TypeId> {
        let mut remap: Vec<TypeId> = Vec::with_capacity(local_items.len());
        for ty in &local_items {
            let canonical = remap_inner_ids(ty, &|id: TypeId| {
                let raw = id.0;
                if raw < base_len {
                    id
                } else {
                    remap[(raw - base_len) as usize]
                }
            });
            remap.push(self.alloc(canonical));
        }
        remap
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

    // P28.2
    /// Rewrite every stored [`TypeId`] through `map`. Used by the
    /// post-merge remap to canonicalize locally-minted TypeIds back to
    /// project-arena ids after a per-thread body walk.
    pub fn remap_typeids(&mut self, map: &dyn Fn(TypeId) -> TypeId) {
        for id in self.named.values_mut() {
            *id = map(*id);
        }
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
pub fn is_assignable_to<V: TypeView + ?Sized>(arena: &V, from: TypeId, to: TypeId) -> bool {
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
        (TypeKind::Named { name: na }, TypeKind::Named { name: nb }) => na == nb,
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
            na == nb
                && aa.len() == ab.len()
                && aa.iter().zip(ab).all(|(x, y)| {
                    if *x == *y {
                        return true;
                    }
                    is_assignable_to(arena, *x, *y) && is_assignable_to(arena, *y, *x)
                })
        }
        // **P19.14** — `Generic<N, args>` assigns to `Named<N>` (raw
        // type form). Captures `nodeTime<float>` → `nodeTime` (no
        // generic args declared) which appears in stdlib / library
        // signatures as a wildcard receiver.
        (TypeKind::Generic { name: na, .. }, TypeKind::Named { name: nb }) if na == nb => true,
        // P7.5 anonymous structural compat: a value of `{a: A, b: B}`
        // is assignable to `{a: A}` (width subtyping — source may have
        // *extra* fields). Each shared field's source type must be
        // assignable to the target's field type.
        (TypeKind::Anonymous { fields: fa }, TypeKind::Anonymous { fields: fb }) => {
            fb.iter().all(|(name, want)| {
                fa.iter()
                    .find(|(n, _)| n == name)
                    .is_some_and(|(_, got)| is_assignable_to(arena, *got, *want))
            })
        }
        (TypeKind::Tuple { elements: ea }, TypeKind::Tuple { elements: eb }) => {
            ea.len() == eb.len()
                && ea
                    .iter()
                    .zip(eb)
                    .all(|(x, y)| is_assignable_to(arena, *x, *y))
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
        _ => false,
    }
}

/// Primitive widening lattice: `int -> float`, plus identity. Strings,
/// chars, bools etc. don't widen.
/// `true` for any of the runtime "node-tag" generic names that
/// auto-deref to their inner type in the assignability relation
///. Drawn from the TS reference's `StdCoreTypes` interface.
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
    ///
    // P28.1
    /// Generic over [`TypeArenaMut`] so callers can pass either the
    /// canonical [`TypeArena`] or a per-thread [`LocalArena`].
    pub fn substitute<V: TypeArenaMut + ?Sized>(&self, arena: &mut V, ty: TypeId) -> TypeId {
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
                    let mut new_t = arena.alloc(Type {
                        kind: TypeKind::Generic {
                            name,
                            args: new_args,
                        },
                        nullable: false,
                    });
                    if t.nullable {
                        new_t = arena.nullable(new_t);
                    }
                    new_t
                }
            }
            TypeKind::Tuple { elements } => {
                let new_els: Vec<TypeId> = elements
                    .iter()
                    .map(|e| self.substitute(arena, *e))
                    .collect();
                if new_els == *elements {
                    ty
                } else {
                    let mut new_t = arena.alloc(Type {
                        kind: TypeKind::Tuple { elements: new_els },
                        nullable: false,
                    });
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
pub fn is_castable<V: TypeView + ?Sized>(arena: &V, from: TypeId, to: TypeId) -> bool {
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
        _ => is_assignable_to(arena, from, to),
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

pub fn display<V: TypeView + ?Sized>(arena: &V, id: TypeId) -> String {
    let ty = arena.get(id);
    let mut s = match &ty.kind {
        TypeKind::Null => "null".to_string(),
        TypeKind::Any => "any".to_string(),
        TypeKind::Never => "never".to_string(),
        TypeKind::Primitive(p) => p.name().to_string(),
        TypeKind::Named { name } => name.to_string(),
        TypeKind::Generic { name, args } => {
            let parts: Vec<String> = args.iter().map(|a| display(arena, *a)).collect();
            format!("{name}<{}>", parts.join(", "))
        }
        TypeKind::GenericParam { name, .. } => name.to_string(),
        TypeKind::Lambda(l) => {
            let parts: Vec<String> = l.params.iter().map(|p| display(arena, *p)).collect();
            format!("({}) -> {}", parts.join(", "), display(arena, l.ret))
        }
        TypeKind::Tuple { elements } => {
            let parts: Vec<String> = elements.iter().map(|e| display(arena, *e)).collect();
            format!("({})", parts.join(", "))
        }
        TypeKind::Anonymous { fields } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, t)| format!("{n}: {}", display(arena, *t)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        TypeKind::Enum { name, .. } => name.to_string(),
        TypeKind::Union { alts } => {
            let parts: Vec<String> = alts.iter().map(|a| display(arena, *a)).collect();
            parts.join(" | ")
        }
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Any) {
        s.push('?');
    }
    s
}

// P18.1
/// Fully-qualified-name display, matching the TS reference's
/// canonical printer (e.g. `core::int`, `core::Array<core::int | null>`,
/// `project::Foo`).
///
/// `home_lib` resolves a Named/Generic/Enum's home module (e.g. `Foo →
/// "project"`, `node → "core"`). Returning `None` falls back to the
/// `core` library — matches TS's behavior for builtins not in the
/// project decl table.
///
/// Differences from [`display`]:
/// - Primitives, builtin runtime types, and unresolved names get a
///   `core::` prefix.
/// - User types resolve to `<lib>::<Name>` via `home_lib`.
/// - `nullable` is rendered as ` | null` instead of the `?` suffix.
/// - `any` (always nullable) is rendered as `core::any | null`.
pub fn display_fqn<V: TypeView + ?Sized>(
    arena: &V,
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
        TypeKind::Tuple { elements } => {
            let parts: Vec<String> = elements
                .iter()
                .map(|e| display_fqn(arena, *e, home_lib))
                .collect();
            format!("({})", parts.join(", "))
        }
        TypeKind::Anonymous { fields } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, t)| format!("{n}: {}", display_fqn(arena, *t, home_lib)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        TypeKind::Enum { name, .. } => format!(
            "{}::{}",
            home_lib(name.as_str()).unwrap_or_else(|| "core".to_string()),
            name
        ),
        TypeKind::Union { alts } => {
            let parts: Vec<String> = alts
                .iter()
                .map(|a| display_fqn(arena, *a, home_lib))
                .collect();
            parts.join(" | ")
        }
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null) {
        s.push_str(" | null");
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
        assert_eq!(display(&a, int_q), "int?");
        assert_eq!(display(&a, arr), "Array<String>");
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

    #[test]
    fn anonymous_width_subtyping() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let str_t = a.primitive(Primitive::String);
        let two = a.alloc(Type {
            kind: TypeKind::Anonymous {
                fields: vec![("a".into(), int), ("b".into(), str_t)],
            },
            nullable: false,
        });
        let one = a.alloc(Type {
            kind: TypeKind::Anonymous {
                fields: vec![("a".into(), int)],
            },
            nullable: false,
        });
        // {a, b} → {a}  (width subtyping: extra field b is fine)
        assert!(is_assignable_to(&a, two, one));
        // {a} → {a, b}  is NOT — would be missing field b.
        assert!(!is_assignable_to(&a, one, two));
    }

    // P28.1
    #[test]
    fn local_arena_returns_canonical_id_for_base_types() {
        let mut canon = fresh();
        let int_canon = canon.primitive(Primitive::Int);
        let local = LocalArena::wrap(&canon);
        // Local view sees the canonical int by base-prefix lookup.
        assert_eq!(
            local.get(int_canon).kind,
            TypeKind::Primitive(Primitive::Int)
        );
        assert_eq!(local.base_len(), canon.len() as u32);
    }

    // P28.1
    #[test]
    fn local_arena_dedups_against_canonical_base() {
        let mut canon = fresh();
        let int_canon = canon.primitive(Primitive::Int);
        let mut local = LocalArena::wrap(&canon);
        // Allocating an Int via LocalArena returns the *canonical* id —
        // no new local-tail entry, because the base intern already has it.
        let int_local = local.primitive(Primitive::Int);
        assert_eq!(int_local, int_canon);
        assert_eq!(local.local_len(), 0);
    }

    // P28.1
    #[test]
    fn local_arena_appends_new_types_past_base() {
        let mut canon = fresh();
        let _ = canon.primitive(Primitive::Int);
        let base_len = canon.len() as u32;
        let mut local = LocalArena::wrap(&canon);
        let foo = local.named("Foo");
        assert_eq!(foo.raw(), base_len);
        assert_eq!(local.local_len(), 1);
        // Second mint of the same name dedups against local intern.
        let foo2 = local.named("Foo");
        assert_eq!(foo, foo2);
        assert_eq!(local.local_len(), 1);
        // get works for both base and local TypeIds.
        let int_canon = TypeId::from_raw(0);
        assert!(matches!(local.get(int_canon).kind, TypeKind::Primitive(_)));
        let TypeKind::Named { name } = &local.get(foo).kind else {
            panic!("expected Named");
        };
        assert_eq!(name, "Foo");
    }

    // P28.1
    #[test]
    fn merge_local_canonicalizes_new_types_in_one_pass() {
        // Two threads each mint `Array<int>` against a shared base.
        // After merging both into canonical, both threads' remap maps
        // their local `Array<int>` TypeId to the same canonical id.
        let mut canon = fresh();
        seed_primitives(&mut canon);
        let int_canon = canon.lookup(&Type {
            kind: TypeKind::Primitive(Primitive::Int),
            nullable: false,
        });
        assert!(int_canon.is_some());

        let thread_a_local: LocalArena<'_> = {
            let mut la = LocalArena::wrap(&canon);
            let int = la.primitive(Primitive::Int);
            assert_eq!(int, int_canon.unwrap());
            let _arr = la.generic("Array", vec![int]);
            la
        };
        let thread_b_local: LocalArena<'_> = {
            let mut la = LocalArena::wrap(&canon);
            let int = la.primitive(Primitive::Int);
            let _arr = la.generic("Array", vec![int]);
            la
        };

        let a_base_len = thread_a_local.base_len();
        let b_base_len = thread_b_local.base_len();
        let a_items = thread_a_local.into_local_items();
        let b_items = thread_b_local.into_local_items();

        // Both threads minted exactly one new entry: Array<int>.
        assert_eq!(a_items.len(), 1);
        assert_eq!(b_items.len(), 1);

        let remap_a = canon.merge_local(a_base_len, a_items);
        let remap_b = canon.merge_local(b_base_len, b_items);

        // Both threads' Array<int> canonicalize to the same TypeId.
        assert_eq!(remap_a[0], remap_b[0]);

        // The canonical arena has exactly one Array<int> entry.
        let canon_array = canon.lookup(&Type {
            kind: TypeKind::Generic {
                name: "Array".into(),
                args: smallvec::smallvec![int_canon.unwrap()],
            },
            nullable: false,
        });
        assert_eq!(canon_array, Some(remap_a[0]));
    }

    // P28.1
    #[test]
    fn merge_local_remaps_nested_local_typeids() {
        // Local mints Bar then Generic<Foo, [Bar]>. Bar lives in the
        // local tail, so the Generic's args[0] points at a local TypeId.
        // Merge must remap the inner reference correctly even when
        // canonical does not yet contain Bar.
        let canon = fresh();
        let mut la = LocalArena::wrap(&canon);
        let bar = la.named("Bar");
        let _foo_bar = la.generic("Foo", vec![bar]);
        let base_len = la.base_len();
        let items = la.into_local_items();
        assert_eq!(items.len(), 2);

        let mut canon = canon;
        let remap = canon.merge_local(base_len, items);
        // canonical[remap[1]] must be Generic { name: "Foo", args: [remap[0]] }.
        let canonical_foo = canon.get(remap[1]);
        let TypeKind::Generic { name, args } = &canonical_foo.kind else {
            panic!("expected Generic");
        };
        assert_eq!(name, "Foo");
        assert_eq!(args.as_slice(), &[remap[0]]);
        // And canonical[remap[0]] is Named("Bar").
        let canonical_bar = canon.get(remap[0]);
        let TypeKind::Named { name } = &canonical_bar.kind else {
            panic!("expected Named");
        };
        assert_eq!(name, "Bar");
    }

    // Helper for the merge tests above: seed the canonical arena with
    // the same primitives the analyzer's `seed_builtins` would, so the
    // merge tests start from a realistic baseline.
    fn seed_primitives(arena: &mut TypeArena) {
        for p in [
            Primitive::Bool,
            Primitive::Int,
            Primitive::Float,
            Primitive::Char,
            Primitive::String,
            Primitive::Time,
            Primitive::Duration,
            Primitive::Geo,
        ] {
            arena.primitive(p);
        }
        arena.null();
        arena.any();
        arena.never();
    }

    // P28.1
    #[test]
    fn local_arena_substitute_matches_canonical() {
        // Same Type::Generic in canonical and via LocalArena must
        // produce the same canonical TypeId after merge — the
        // SmolStr/&str dedup anchor (P25.4) extended to the merge path.
        let mut canon = fresh();
        seed_primitives(&mut canon);
        let int_canon = canon.primitive(Primitive::Int);
        let arr_canon = canon.generic("Array", vec![int_canon]);

        let mut la = LocalArena::wrap(&canon);
        // Same shape minted via LocalArena should dedup to the same id
        // since base already has it.
        let arr_local = la.generic("Array", vec![int_canon]);
        assert_eq!(arr_local, arr_canon);
        assert_eq!(la.local_len(), 0);
    }

    // P28.1
    /// Anchor that `LocalArena: Send` — required for rayon workers
    /// to own one. Compile-time assertion via a free fn.
    #[test]
    fn local_arena_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<LocalArena<'_>>();
    }
}
