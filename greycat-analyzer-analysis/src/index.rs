use rustc_hash::{FxHashMap, FxHashSet};

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{
    ItemId, Primitive, Symbol, SymbolTable, Type, TypeArena, TypeId, TypeKind,
};
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Annotation, Decl, Expr, TypeAttr};
use greycat_analyzer_hir::{DeclRegistry, Hir};

use crate::well_known::WellKnown;

/// Runtime-exposed value-position globals. The runtime
/// makes these names available at value position with a fixed type;
/// they have no `.gcl` declaration, so the resolver/analyzer must seed
/// them. `(name, primitive)` pairs — extend as new runtime globals are
/// confirmed against `greycat run`.
pub const BUILTIN_RUNTIME_GLOBALS: &[(&str, Primitive)] =
    &[("Infinity", Primitive::Float), ("NaN", Primitive::Float)];

/// Hard cap on supertype-chain depth. The GreyCat runtime rejects any
/// declaration whose `extends` chain reaches a 5th level with
/// `too depth inheritance: <name>` (verified against `greycat build`:
/// four-level `A <- B <- C <- D` builds clean, five-level
/// `A <- B <- C <- D <- E` errors out). Walkers that follow
/// `supertype` cap their iteration at this value both as a defense
/// against accidental cycles in in-progress source and to match the
/// runtime's actual limit. Set to the limit itself (4) rather than
/// `limit - 1` so that legal chains are always reachable even when
/// the walk starts at the deepest descendant.
const MAX_SUPERTYPE_CHAIN_DEPTH: usize = 4;

/// Symbol namespace for top-level decls. The GreyCat runtime
/// (validated against `greycat build` 8.0.301-dev) keeps three name
/// slots — type/enum, fn, and module-var (root node) — that may all
/// share an identifier. `type geo` (type-ns) and `fn geo(...)`
/// (fn-ns) coexist in `lib/std/core.gcl`; the runtime probe confirms
/// every cross-namespace pair builds clean, while every in-namespace
/// pair errors at parse time.
///
/// Per-name indexes that gate `duplicate-decl` / `ambiguous-symbol`
/// filter by this tag; `Decl::Pragma` has no namespace (returns
/// `None` from [`Namespace::of_decl`]) and is skipped at every
/// callsite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Namespace {
    /// `type` / `enum` declarations.
    Type,
    /// Module-level `fn` declarations (not type methods).
    Fn,
    /// Module-level `var` declarations (graph-root nodes).
    Var,
}

impl Namespace {
    /// Returns the `Namespace` variant of the given decl
    pub fn of_decl(decl: &Decl) -> Option<Self> {
        match decl {
            Decl::Type(_) | Decl::Enum(_) => Some(Self::Type),
            Decl::Fn(_) => Some(Self::Fn),
            Decl::Var(_) => Some(Self::Var),
            Decl::Pragma(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeclLocation {
    pub uri: Uri,
    pub id: Idx<Decl>,
    pub ns: Namespace,
}

/// Cross-module project context: name tables, structure indices, and native-fn signatures shared across module ingestion.
/// The shared `TypeArena` lives in `ProjectAnalysis` and is threaded through `ingest` so all type allocations land in
/// one arena. Public lookup helpers (`has_name`, `locate_decl`, `type_members_for`, etc.) keep a `&str` API and
/// translate to `Symbol` internally.
#[derive(Debug, Default)]
pub struct ProjectIndex {
    /// Project-wide string interner.
    ///
    /// Owns canonical storage for every name the analyzer looks up across modules.
    pub symbols: SymbolTable,
    /// Set of public top-level declared type/enum names.
    pub type_names: FxHashSet<Symbol>,
    /// Set of public top-level declared variable names.
    pub var_names: FxHashSet<Symbol>,
    /// Set of public top-level declared function names.
    pub fn_names: FxHashSet<Symbol>,
    /// Private-modifier decl locations
    ///
    /// Cross-module bare-name resolution skips these, but FQN lookup still reaches them.
    pub private_locations: FxHashSet<(Uri, Idx<Decl>)>,
    /// Cross-module decl table: name -> all `(Uri, Idx<Decl>, Namespace)` triples
    ///
    /// Collisions kept, disambiguation at use-site.
    pub decl_locations: FxHashMap<Symbol, Vec<DeclLocation>>,
    /// `@expose`-renamed -> exposure sites
    ///
    /// Lets lints ask "is this name part of the runtime API?".
    pub exposed: FxHashMap<Symbol, Vec<ExposureSite>>,
    /// Per-type flag bits from `@iterable` / `@deref` / `@primitive` annotations, keyed by declared type name.
    pub type_flags: FxHashMap<ItemId, TypeFlags>,
    /// Per-module `@permission("name")` pragmas for capability checks.
    pub module_permissions: FxHashMap<Uri, FxHashSet<Symbol>>,
    /// Module-name -> URI
    ///
    /// Lets the resolver recognize module prefixes in `module::Decl` expressions.
    pub module_names: FxHashMap<Symbol, Uri>,
    /// Stem-colliding duplicate modules excluded from the project closure
    ///
    /// Overlaid with a `duplicate-module-name` diagnostic.
    pub duplicate_modules: FxHashMap<Uri, (Symbol, Uri)>,
    /// Cross-module structure index keyed by `ItemId`
    ///
    /// Maps each type to its home URI and attr/method name -> HIR index.
    pub type_members: FxHashMap<ItemId, TypeMembers>,
    /// Pre-lowered top-level fn signatures keyed by `ItemId`
    ///
    /// `return_ty` is already in the shared arena for call-site substitution.
    pub fn_signatures: FxHashMap<ItemId, FnSignature>,
    /// Enum types pre-registered in the shared arena
    ///
    /// Lets the analyzer type `module::Enum::variant` as the correct enum rather than `any`.
    pub enum_types: FxHashMap<ItemId, TypeId>,
    /// Pre-lowered top-level `var` declared types keyed by `ItemId`
    ///
    /// Enables inline typing of cross-module var references at body-walk time.
    pub var_types: FxHashMap<ItemId, TypeId>,
    /// Runtime-only globals (`Infinity`, `NaN`, `-Infinity`) and their declared types
    ///
    /// Seeded by `seed_builtin_names`.
    pub runtime_globals: FxHashMap<Symbol, TypeId>,
    /// `ItemId`s of `abstract`-modifier types
    ///
    /// Consulted by the sealed-hierarchy narrowing pass.
    pub is_abstract: FxHashSet<ItemId>,
    /// Per-type sorted closure of concrete leaves reachable through `extends`
    ///
    /// Built by `populate_subtype_indices`.
    pub subtype_closure: FxHashMap<ItemId, Box<[ItemId]>>,
    /// Reverse index: closure-set -> abstract ancestor `ItemId`
    ///
    /// Lets `narrow_complement` collapse unions back to the abstract type.
    pub abstract_by_closure_set: FxHashMap<Box<[ItemId]>, ItemId>,
    /// Total modules ingested
    ///
    /// Useful for "did stdlib load?" smoke checks at the LSP boundary.
    pub modules_ingested: usize,
    /// Canonical `ItemId`s for the 8 native-core primitives, set in
    /// `with_symbols`. `None` only on a `default()`-constructed index;
    /// every real index goes through `with_symbols`.
    builtins: Option<Builtins>,
}

/// Canonical `ItemId` per native-core primitive. A primitive `int` is
/// `Type(ItemId(core, int))`; this holds those handles. Std-free: an
/// `ItemId` is two interned symbols, valid with or without std loaded.
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
}

impl Builtins {
    fn compute(symbols: &SymbolTable) -> Self {
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
        }
    }
}

/// Pre-lowered top-level fn signature; `return_ty` is a `TypeId` in the shared arena and may be
/// `GenericParam` for generic fns. Consulted by `try_member_call_typing` for cross-module `Ident`
/// callees and `module::fn` qualified-static shapes.
#[derive(Debug, Clone)]
pub struct FnSignature {
    pub home_uri: Uri,
    /// `None` when the source decl has no explicit return type
    ///
    /// Consumers fall back to `any_nullable` at the use site.
    pub return_ty: Option<TypeId>,
    /// Interned generic param names
    ///
    /// Resolve to text via `ProjectIndex::symbols`.
    pub generics: Vec<Symbol>,
    /// Pre-lowered parameter types in declared order
    ///
    /// Enables generic-call inference for cross-module callees.
    pub params: Vec<TypeId>,
    /// `true` when the runtime erases this generic fn's result to `any?`
    ///
    /// Drives the `generic-erasure` diagnostic.
    pub return_erases: bool,
}

/// Per-type cross-module member index with pre-lowered attr/method types in the shared project arena.
/// Lets the analyzer type `recv.attr` / `recv.method()` inline via `arena.substitute` without crossing
/// the foreign HIR boundary.
#[derive(Debug, Clone)]
pub struct TypeMembers {
    /// URI of the module that declared this type.
    pub home_uri: Uri,
    /// Attr name -> HIR index.
    pub attrs: FxHashMap<Symbol, Idx<TypeAttr>>,
    /// Attr names in declaration order
    ///
    /// Source of truth for "missing required fields" diagnostic wording.
    pub attr_order: Box<[Symbol]>,
    /// Method name -> HIR index.
    pub methods: FxHashMap<Symbol, Idx<Decl>>,
    /// Ordered generic param names (`type Map<K, V>` -> `[K, V]`)
    ///
    /// Used to build substitution maps at member-access/call sites.
    pub generics: Vec<Symbol>,
    /// Pre-lowered attr declared types; `TypeId`s reference the shared arena
    ///
    /// Generics use `GenericParam` pending call-site substitution.
    pub attr_types: FxHashMap<Symbol, TypeId>,
    /// Pre-lowered method return types
    ///
    /// Same arena/substitution semantics as `attr_types` absent for methods without explicit return types.
    pub method_returns: FxHashMap<Symbol, TypeId>,
    /// Full pre-lowered signatures for generic methods only
    ///
    /// Enables call-site inference without crossing the foreign HIR boundary.
    pub method_signatures: FxHashMap<Symbol, FnSignature>,
    /// Names of `static`-modifier attrs
    ///
    /// Distinguishes `Foo::path` (value typed as `String`) from a non-static field handle.
    pub static_attrs: FxHashSet<Symbol>,
    /// Names of `private`-modifier attrs
    ///
    /// The assignment checker emits `private-attr-write` for writes outside the constructor.
    pub private_attrs: FxHashSet<Symbol>,
    /// Names of `static`-modifier methods
    ///
    /// Filtered out of instance-access resolution by `resolve_member`.
    pub static_methods: FxHashSet<Symbol>,
    /// Names of `abstract`-modifier methods
    ///
    /// Lets the LSP declaration handler walk the supertype chain without fetching foreign HIR.
    pub abstract_methods: FxHashSet<Symbol>,
    /// Direct supertype `ItemId`
    ///
    /// Drives inheritance lookup and `Type(Sub)` -> `Type(Super)` assignability.
    pub supertype: Option<ItemId>,
    /// Instantiated supertype `TypeId` in the shared arena (e.g. `Generic { decl: Base, args: [int] }`);
    /// enables generic-arg-substituted assignability chain walks.
    pub supertype_ty: Option<TypeId>,
    /// Cached pre-lowered return `TypeId` of the `@deref`-annotated method
    ///
    /// Lets `arrow_deref_receiver` resolve `*n` / `n->m()` with a single field read.
    pub deref_return_ty: Option<TypeId>,
}

/// Annotation-derived flag bits on a type declaration.
///
/// - `iterable`: `for x in t` is legal when `t` is this type.
/// - `deref`: `Some("method")` when the type carries
///   `@deref("method")`; member access on this type can also resolve
///   through the named method's return type when the property isn't
///   on the type itself. `None` for types without `@deref`.
/// - `primitive`: exempts the type from structural-compatibility
///   rules — assigned only by exact-name match.
///
/// Wiring these into the analyzer's behavior happens incrementally
/// today they're a populated data table the analyzer can read; full
/// `for-in` legality / member-resolution-through-deref / primitive-
/// strict-equality semantics are downstream follow-ups.
#[derive(Debug, Default, Clone)]
pub struct TypeFlags {
    pub iterable: bool,
    pub deref: Option<Symbol>,
    pub primitive: bool,
}

/// A single `@expose`-annotated decl, recorded for the
/// runtime-API surface. `local_name` is the source-level name in the
/// declaring module; `rename` is what `@expose("renamed")` gave it
/// (or `None` when `@expose` was used bare). Both are interned
/// through the project's [`SymbolTable`] — names are dedup'd, and
/// repeated `@expose("api_v1")` annotations across decls share one
/// `Symbol`.
#[derive(Debug, Clone)]
pub struct ExposureSite {
    pub uri: Uri,
    pub decl: Idx<Decl>,
    pub local_name: Symbol,
    pub rename: Option<Symbol>,
}

impl ProjectIndex {
    /// Construct an empty index with builtin type / runtime-global
    /// names seeded into the symbol table. The shared `TypeArena`
    /// receives the primitive / global TypeIds — primitives only need
    /// interning so subsequent `arena.primitive(p)` calls return the
    /// canonical IDs, and runtime globals (`Infinity`, `NaN`) need a
    /// concrete `TypeId` so the body walker can consume them.
    pub fn new(arena: &mut TypeArena) -> Self {
        Self::with_symbols(SymbolTable::new(), arena)
    }

    /// Construct a fresh index that reuses an existing
    /// [`SymbolTable`]. Lets `ProjectAnalysis::invalidate` rebuild
    /// the per-module index without invalidating the `Symbol`s held
    /// elsewhere (notably the per-stage signature cache).
    pub fn with_symbols(symbols: SymbolTable, arena: &mut TypeArena) -> Self {
        let mut idx = Self {
            symbols,
            ..Self::default()
        };
        seed_builtin_names(&mut idx.symbols, &mut idx.type_names);
        idx.builtins = Some(Builtins::compute(&idx.symbols));
        // Runtime-exposed value-position globals
        // (`Infinity`, `NaN`). Registered here (not in
        // `seed_builtin_names`) because they're values, not types,
        // and they need a typed `TypeId` the body walker can consume.
        for (name, prim) in BUILTIN_RUNTIME_GLOBALS {
            let sym = idx.symbols.intern(name);
            let ty = arena.primitive(*prim);
            idx.runtime_globals.insert(sym, ty);
        }
        idx
    }

    /// Read-only `&str` → [`Symbol`] lookup. Returns
    /// `None` if `name` was never interned. Hot lookup paths use
    /// this to avoid mutating the symbol table from a `&self`
    /// borrow.
    pub fn symbol(&self, name: &str) -> Option<Symbol> {
        self.symbols.lookup(name)
    }

    /// Canonical primitive `ItemId`s (see [`Builtins`]). Panics only on
    /// a `default()`-constructed index; all real indices go through
    /// `new` / `with_symbols`.
    pub fn builtins(&self) -> &Builtins {
        self.builtins
            .as_ref()
            .expect("ProjectIndex::builtins on a default-constructed index")
    }

    /// Build an [`ItemId`] for `(uri, name)`. Returns `None` if `uri`
    /// doesn't have a recognisable module-name stem. Cheap (one
    /// `module_name_from_uri` call + one symbol intern). Use this
    /// anywhere you have a URI and an item-name symbol and need the
    /// composed identity for `decl_registry` / `type_members` / etc.
    pub fn item_id_for(&self, uri: &Uri, name: Symbol) -> Option<ItemId> {
        let module_sym = self.symbols.intern(module_name_from_uri(uri)?);
        Some(ItemId::new(module_sym, name))
    }

    /// Walk the supertype chain starting at `type_name`,
    /// returning the first `TypeMembers` entry that contains the
    /// member matched by `pred`. Used to find inherited attrs /
    /// methods (`pvInstallation->timezone` resolves through
    /// `PVInstallation extends PVEntity`'s `timezone: TimeZone`).
    /// Number of types in `type_id`'s supertype chain, counting the
    /// type itself. Returns 0 when `type_id` is unknown. Stops
    /// counting at [`MAX_SUPERTYPE_CHAIN_DEPTH`] + 1 — the caller only
    /// needs to distinguish "within limit" from "exceeds limit".
    pub fn supertype_chain_length(&self, type_id: ItemId) -> usize {
        let mut cur = type_id;
        let mut len: usize = 0;
        for _ in 0..=MAX_SUPERTYPE_CHAIN_DEPTH {
            let Some(members) = self.type_members.get(&cur) else {
                return len;
            };
            len += 1;
            match members.supertype {
                Some(parent) => cur = parent,
                None => return len,
            }
        }
        len
    }

    /// Maximum number of types the runtime accepts in an `extends`
    /// chain (including the leaf type itself). Re-exported so callers
    /// in this crate can mention the limit in user-facing messages
    /// without depending on the private constant.
    pub const MAX_INHERITANCE_DEPTH: usize = MAX_SUPERTYPE_CHAIN_DEPTH;

    /// Bounded at [`MAX_SUPERTYPE_CHAIN_DEPTH`] hops to match the
    /// runtime's inheritance-depth ceiling and defend against accidental
    /// cycles in in-progress source.
    fn walk_member_chain<P>(&self, type_id: ItemId, pred: P) -> Option<&TypeMembers>
    where
        P: FnMut(&TypeMembers) -> bool,
    {
        self.walk_member_chain_with_id(type_id, pred)
            .map(|(_, m)| m)
    }

    /// Same walk as [`walk_member_chain`] but also returns the
    /// `ItemId` of the level where the predicate matched. Callers that
    /// need to know *which* type in the chain owns the member (e.g.
    /// to compare against an enclosing-type identity) use this.
    fn walk_member_chain_with_id<P>(
        &self,
        type_id: ItemId,
        mut pred: P,
    ) -> Option<(ItemId, &TypeMembers)>
    where
        P: FnMut(&TypeMembers) -> bool,
    {
        let mut cur = type_id;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let members = self.type_members.get(&cur)?;
            if pred(members) {
                return Some((cur, members));
            }
            cur = members.supertype?;
        }
        None
    }

    /// `true` iff `child` is the same type as `ancestor` or appears as
    /// a transitive descendant via the `supertype` chain. Bounded by
    /// [`MAX_SUPERTYPE_CHAIN_DEPTH`] like the other chain walks.
    /// Used by `check_private_attr_write` to allow writes from
    /// non-static methods of subtypes that inherit a private attr.
    pub fn type_is_descendant_or_self(&self, child: ItemId, ancestor: ItemId) -> bool {
        let mut cur = child;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            if cur == ancestor {
                return true;
            }
            let Some(members) = self.type_members.get(&cur) else {
                return false;
            };
            cur = match members.supertype {
                Some(s) => s,
                None => return false,
            };
        }
        false
    }

    /// Attr's HIR index, walking the supertype chain. Returns the
    /// `(home_uri, attr_id)` of the type that owns the attr (which
    /// may be the type itself or a parent), so cross-module hover /
    /// goto-def points at the right module.
    pub fn type_attr_id_chain(
        &self,
        type_id: ItemId,
        attr_name: Symbol,
    ) -> Option<(Uri, Idx<TypeAttr>)> {
        let members = self.walk_member_chain(type_id, |m| m.attrs.contains_key(&attr_name))?;
        members
            .attrs
            .get(&attr_name)
            .map(|id| (members.home_uri.clone(), *id))
    }

    /// Find the entry in `type_id`'s supertype chain that owns
    /// `attr_name` (the first hop that has the attr in `attrs`).
    /// Returns the owning level's `ItemId` alongside the members so
    /// callers can identify *which* type declared the attr — needed
    /// by `check_private_attr_write` to compare against the enclosing
    /// method's owner. `private` is sourced from the declaration,
    /// not the use site.
    pub fn walk_chain_for_private_attr(
        &self,
        type_id: ItemId,
        attr_name: Symbol,
    ) -> Option<(ItemId, &TypeMembers)> {
        self.walk_member_chain_with_id(type_id, |m| m.attrs.contains_key(&attr_name))
    }

    /// Method's HIR index, walking the supertype chain.
    pub fn type_method_id_chain(
        &self,
        type_id: ItemId,
        method_name: Symbol,
    ) -> Option<(Uri, Idx<Decl>)> {
        let members = self.walk_member_chain(type_id, |m| m.methods.contains_key(&method_name))?;
        members
            .methods
            .get(&method_name)
            .map(|id| (members.home_uri.clone(), *id))
    }

    /// Like [`type_method_id_chain`] but skips chain levels where the
    /// candidate is a `static` method. Used by instance-access member
    /// resolution (`recv.method` / `recv->method`) — the runtime
    /// resolves those against *instance* methods only, so a parent's
    /// non-static method should not be shadowed by a subtype's static
    /// method of the same name.
    pub fn type_instance_method_id_chain(
        &self,
        type_id: ItemId,
        method_name: Symbol,
    ) -> Option<(Uri, Idx<Decl>)> {
        let members = self.walk_member_chain(type_id, |m| {
            m.methods.contains_key(&method_name) && !m.static_methods.contains(&method_name)
        })?;
        members
            .methods
            .get(&method_name)
            .map(|id| (members.home_uri.clone(), *id))
    }

    /// Walk the *strict* supertype chain of `type_id` (skipping the
    /// type itself) looking for an ancestor that declares
    /// `method_name` with the `abstract` modifier. Returns
    /// `(home_uri, Idx<Decl>)` of the abstract declaration, or `None`
    /// if no abstract ancestor exists. Powers
    /// `textDocument/declaration`: the inverse of
    /// `textDocument/implementation`.
    pub fn find_abstract_ancestor_method(
        &self,
        type_id: ItemId,
        method_name: Symbol,
    ) -> Option<(Uri, Idx<Decl>)> {
        let start_members = self.type_members.get(&type_id)?;
        let mut cur = start_members.supertype?;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let members = self.type_members.get(&cur)?;
            if members.abstract_methods.contains(&method_name)
                && let Some(decl_id) = members.methods.get(&method_name)
            {
                return Some((members.home_uri.clone(), *decl_id));
            }
            cur = members.supertype?;
        }
        None
    }

    /// Pre-lowered attr type, walking the supertype chain. The
    /// `TypeId` lives in the project arena and may reference
    /// `GenericParam(T, owner=parent_type)` if the attr is declared
    /// on a generic parent.
    pub fn type_attr_ty_chain(&self, type_id: ItemId, attr_name: Symbol) -> Option<TypeId> {
        let members = self.walk_member_chain(type_id, |m| m.attr_types.contains_key(&attr_name))?;
        members.attr_types.get(&attr_name).copied()
    }

    /// Pre-lowered method return type, walking the supertype chain.
    pub fn type_method_return_chain(&self, type_id: ItemId, method_name: Symbol) -> Option<TypeId> {
        let members =
            self.walk_member_chain(type_id, |m| m.method_returns.contains_key(&method_name))?;
        members.method_returns.get(&method_name).copied()
    }

    /// `true` iff `sub` is `sup` or any of its transitive supertypes
    /// is `sup`. Bounded at 32 hops.
    pub fn is_subtype_of(&self, sub: ItemId, sup: ItemId) -> bool {
        if sub == sup {
            return true;
        }
        let mut cur = sub;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let Some(members) = self.type_members.get(&cur) else {
                return false;
            };
            let Some(parent) = members.supertype else {
                return false;
            };
            if parent == sup {
                return true;
            }
            cur = parent;
        }
        false
    }

    // Kept as a thin alias for symmetry with the chain
    // walkers above. `is_subtype_of` now takes `ItemId` directly so
    // the wrapper is purely cosmetic; consumers can call either.
    pub fn is_subtype_of_decl(&self, sub: ItemId, sup: ItemId) -> bool {
        self.is_subtype_of(sub, sup)
    }
    /// `true` iff the decl at `(uri, decl_id)` was ingested with the
    /// `private` modifier. Lets the resolver filter cross-module
    /// candidates by visibility for bare-name lookup while leaving
    /// the FQN path unaffected.
    pub fn is_decl_private(&self, uri: &Uri, decl_id: Idx<Decl>) -> bool {
        self.private_locations.contains(&(uri.clone(), decl_id))
    }

    /// Walk a HIR module's top-level decls and register everything
    /// that's a type-name (type / enum) or a native function. Records
    /// every encountered decl into `decl_registry` (`ItemId →
    /// Idx<Decl>` map) and well-known `(lib, module, name)` slots,
    /// allocates the enum's
    /// `TypeKind::Enum` shape into the shared [`TypeArena`], and
    /// publishes the canonical enum `TypeId` to [`Self::enum_types`]
    /// — so by the time any signature-lowering pass runs,
    /// `enum_type_for(name)` returns the same `TypeId` every
    /// downstream stage will see. Re-entrant: calling twice with the
    /// same `(uri, hir)` is a no-op apart from the counter —
    /// duplicate `(uri, decl_id)` pairs are not appended.
    pub fn ingest(
        &mut self,
        uri: &Uri,
        hir: &Hir,
        arena: &mut TypeArena,
        decl_registry: &mut DeclRegistry,
        well_known: &mut WellKnown,
    ) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        // Capture the module's name (URI's filename stem
        // without `.gcl`) so resolver / pass 3.5 can recognize
        // `module::Decl` chains.
        //
        // Duplicate detection: if another file already claimed this
        // module name, the current file is recorded in
        // `duplicate_modules` and excluded from project ingest (we
        // return before walking the decl list). The LSP / CLI overlay
        // a `duplicate-module-name` Error+UNNECESSARY diagnostic on
        // every duplicate so the user sees the dim treatment and the
        // hard error explaining why the file is excluded. Re-ingest
        // of the same URI for the same module name (LSP invalidate
        // cycle) is idempotent — not a duplicate.
        let Some(stem) = module_name_from_uri(uri) else {
            return;
        };
        let module_sym = self.symbols.intern(stem);
        match self.module_names.get(&module_sym) {
            Some(existing) if existing != uri => {
                self.duplicate_modules
                    .insert(uri.clone(), (module_sym, existing.clone()));
                return;
            }
            _ => {
                self.module_names.insert(module_sym, uri.clone());
                // The duplicate registry is keyed by URI,
                // not module-name. If a file moves from "duplicate"
                // back to "winner" across invalidate cycles (e.g.
                // the previous winner was deleted from the project),
                // clear its duplicate flag so the diagnostic doesn't
                // linger.
                self.duplicate_modules.remove(uri);
            }
        }
        for decl_id in &module.decls {
            let modifiers = match &hir.decls[*decl_id] {
                Decl::Type(td) => {
                    let name_sym = hir.idents[td.name].symbol;
                    // Recognised type name (drives `has_name` and the
                    // sig-cache fingerprint). Private decls aren't
                    // cross-module visible: same-module access goes
                    // through the per-module `out.type_decls` /
                    // `out.registry` paths, which see the private
                    // type from its own HIR without needing it in the
                    // shared name set.
                    if !td.modifiers.private {
                        self.type_names.insert(name_sym);
                    }
                    // Project-wide identity + well-known slot
                    // recording. Folded in from the former standalone
                    // pre-pass in `stage_lower_signatures` so the
                    // project has a single decl-registration point.
                    let item = ItemId::new(module_sym, name_sym);
                    decl_registry.record(item, *decl_id);
                    well_known.record(
                        &self.symbols[module.lib],
                        &self.symbols[module.name],
                        &self.symbols[name_sym],
                        item,
                    );
                    if td.modifiers.abstract_ {
                        self.is_abstract.insert(item);
                    }
                    // Capture @iterable / @deref / @primitive
                    // flag bits into the per-type table.
                    let flags = derive_type_flags(&self.symbols, &td.modifiers.annotations);
                    if flags.iterable || flags.deref.is_some() || flags.primitive {
                        self.type_flags.entry(item).or_insert(flags);
                    }
                    // Populate the member shape index. Keyed
                    // by `(module, name)` so two same-named types in
                    // different modules coexist unambiguously. The
                    // first ingested decl for a given `ItemId` wins
                    // (re-ingest is a no-op). Private types ARE
                    // included — `private` in GreyCat gates only
                    // cross-module *bare-name* resolution (handled
                    // by `type_names` / `private_locations`), not
                    // member shape. Same-module inherited-attr walks
                    // for a `private type Sub extends Public` need
                    // this entry to start from `Sub` and climb the
                    // chain.
                    if !self.type_members.contains_key(&item) {
                        let generics: Vec<Symbol> =
                            td.generics.iter().map(|g| hir.idents[*g].symbol).collect();
                        let attr_order: Box<[Symbol]> = td
                            .attrs
                            .iter()
                            .map(|attr_id| hir.idents[hir.type_attrs[*attr_id].name].symbol)
                            .collect();
                        // Best-guess supertype linkage at ingest:
                        // unqualified `extends Super` is *probably*
                        // same-module, so mint the same-module ItemId
                        // immediately. The `link_supertypes` post-
                        // pass refines for cross-module unqualified
                        // and qualified cases. Skips primitives —
                        // they never form a TypeMembers entry.
                        let supertype_guess = td.supertype.and_then(|tr| {
                            let parent_ref = &hir.type_refs[tr];
                            if !parent_ref.qualifier.is_empty() {
                                return None;
                            }
                            let parent_sym = hir.idents[parent_ref.name].symbol;
                            if matches!(
                                &self.symbols[parent_sym],
                                "bool"
                                    | "int"
                                    | "float"
                                    | "char"
                                    | "String"
                                    | "time"
                                    | "duration"
                                    | "geo"
                                    | "any"
                                    | "null"
                            ) {
                                return None;
                            }
                            Some(ItemId::new(module_sym, parent_sym))
                        });
                        let mut m = TypeMembers {
                            home_uri: uri.clone(),
                            attrs: FxHashMap::default(),
                            attr_order,
                            methods: FxHashMap::default(),
                            generics,
                            // `attr_types` / `method_returns` get filled in by
                            // `ProjectAnalysis::stage_lower_signatures`
                            // after every module is loaded.
                            attr_types: FxHashMap::default(),
                            method_returns: FxHashMap::default(),
                            method_signatures: FxHashMap::default(),
                            static_attrs: FxHashSet::default(),
                            private_attrs: FxHashSet::default(),
                            static_methods: FxHashSet::default(),
                            abstract_methods: FxHashSet::default(),
                            supertype: supertype_guess,
                            // Filled in by `apply_module_contributions`
                            // after signature lowering — see
                            // `populate_deref_caches` in
                            // [`crate::project`].
                            supertype_ty: None,
                            deref_return_ty: None,
                        };
                        for attr_id in &td.attrs {
                            let attr = &hir.type_attrs[*attr_id];
                            let attr_sym = hir.idents[attr.name].symbol;
                            m.attrs.insert(attr_sym, *attr_id);
                            // Capture `static` flag at
                            // ingest time so `Expr::Static` value
                            // typing can distinguish static-attr
                            // value access from a runtime `field`
                            // handle, even for cross-module attrs.
                            if attr.modifiers.static_ {
                                m.static_attrs.insert(attr_sym);
                            }
                            // Capture `private` flag — read-public /
                            // write-private. The body walker checks
                            // this set on assignment LHS to emit
                            // `private-attr-write`.
                            if attr.modifiers.private {
                                m.private_attrs.insert(attr_sym);
                            }
                        }
                        for method_id in &td.methods {
                            if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                                let method_sym = hir.idents[fnd.name].symbol;
                                m.methods.insert(method_sym, *method_id);
                                if fnd.modifiers.static_ {
                                    m.static_methods.insert(method_sym);
                                }
                                if fnd.modifiers.abstract_ {
                                    m.abstract_methods.insert(method_sym);
                                }
                            }
                        }
                        self.type_members.insert(item, m);
                    }
                    self.record_decl_location(name_sym, uri, *decl_id, Namespace::Type);
                    Some(&td.modifiers)
                }
                Decl::Enum(ed) => {
                    let name_sym = hir.idents[ed.name].symbol;
                    let is_private = ed.modifiers.private;
                    if !is_private {
                        self.type_names.insert(name_sym);
                    }
                    // Project-wide decl handle (enums get one too — the
                    // resolver / lowering paths route foreign enum refs
                    // through `Type(handle)` for some shapes).
                    let enum_item = ItemId::new(module_sym, name_sym);
                    decl_registry.record(enum_item, *decl_id);
                    // Alloc the canonical `TypeKind::Enum` into the
                    // shared arena and publish to `enum_types`
                    // immediately. Doing it here (rather than as a
                    // side effect of `lower_module_signatures`'
                    // `apply_module_contributions`) ensures every
                    // downstream lowering pass — including method
                    // return-type lowering for methods declared in
                    // the same module, and qualified-static value
                    // typing for `mod::PrivColor::Variant` access —
                    // sees the canonical TypeId. Private enums are
                    // included: `private` gates cross-module *bare-
                    // name* resolution, not the canonical shape;
                    // FQN access to a private enum's variants needs
                    // `enum_types` populated to recognise them.
                    self.enum_types.entry(enum_item).or_insert_with(|| {
                        let variants: Box<[Symbol]> = ed
                            .fields
                            .iter()
                            .map(|f| hir.idents[hir.enum_fields[*f].name].symbol)
                            .collect();
                        arena.alloc(Type {
                            kind: TypeKind::Enum {
                                name: name_sym,
                                variants,
                            },
                            nullable: false,
                        })
                    });
                    self.record_decl_location(name_sym, uri, *decl_id, Namespace::Type);
                    Some(&ed.modifiers)
                }
                Decl::Fn(fnd) => {
                    let name_sym = hir.idents[fnd.name].symbol;
                    if !fnd.modifiers.private {
                        self.fn_names.insert(name_sym);
                    }
                    self.record_decl_location(name_sym, uri, *decl_id, Namespace::Fn);
                    Some(&fnd.modifiers)
                }
                Decl::Var(vd) => {
                    let name_sym = hir.idents[vd.name].symbol;
                    if !vd.modifiers.private {
                        self.var_names.insert(name_sym);
                    }
                    self.record_decl_location(name_sym, uri, *decl_id, Namespace::Var);
                    Some(&vd.modifiers)
                }
                Decl::Pragma(p) => {
                    // Capture `@permission("name")` mod-pragmas
                    // into the project-wide `module_permissions` map.
                    if &self.symbols[hir.idents[p.name].symbol] == "permission"
                        && let Some(arg_expr) = p.args.first()
                        && let Expr::String(s) = &hir.exprs[*arg_expr]
                    {
                        let perm_sym = self.symbols.intern(&s.raw_value());
                        self.module_permissions
                            .entry(uri.clone())
                            .or_default()
                            .insert(perm_sym);
                    }
                    None
                }
            };
            // Walk modifiers' annotations for `@expose("name")`
            // and capture the rename target into the project-wide
            // exposed map.
            if let Some(modifiers) = modifiers {
                // Tag private decls so the resolver's
                // bare-name lookup can filter them out of the
                // cross-module candidate set. The decl stays in
                // `decl_locations` (the FQN path still needs to
                // reach it — see probe p5).
                if modifiers.private {
                    self.private_locations.insert((uri.clone(), *decl_id));
                }
                let local_name = hir.decls[*decl_id]
                    .name()
                    .map(|n| hir.idents[n].symbol)
                    .unwrap_or_else(|| self.symbols.intern(""));
                for ann in &modifiers.annotations {
                    if &self.symbols[ann.name.symbol] != "expose" {
                        continue;
                    }
                    let rename = ann.first_string_arg();
                    // Map key: public name. `@expose("renamed")` →
                    // `renamed`; bare `@expose` → the local name.
                    let key_sym = rename.unwrap_or(local_name);
                    let entries = self.exposed.entry(key_sym).or_default();
                    let already = entries
                        .iter()
                        .any(|s| s.uri == *uri && s.decl == *decl_id && s.rename == rename);
                    if !already {
                        entries.push(ExposureSite {
                            uri: uri.clone(),
                            decl: *decl_id,
                            local_name,
                            rename,
                        });
                    }
                }
            }
        }
        self.modules_ingested += 1;
    }

    /// Resolves a name to its item id via the project's decl table and registry.
    ///
    /// This honors the resolving logic of GreyCat. Local first, then cross-module.
    pub fn resolve_item(
        &self,
        decl_registry: &DeclRegistry,
        from_uri: Option<&Uri>,
        name: Symbol,
    ) -> Option<ItemId> {
        // Type-namespace only: this mints an [`ItemId`], so a
        // same-named `Fn` / `Var` decl must never be returned.
        //
        // Two-pass: same-module candidates first (unfiltered — private
        // is visible from within its own module), then cross-module
        // non-private candidates (private gets filtered to mirror the
        // resolver's `is_decl_private` rule in `record_use`).
        if let Some(cur) = from_uri {
            for (uri, _) in self.locate_decl_in_ns(name, Namespace::Type) {
                if uri == cur
                    && let Some(item) = self.item_id_for(uri, name)
                    && decl_registry.lookup(item).is_some()
                {
                    return Some(item);
                }
            }
        }
        for (uri, decl) in self.locate_decl_in_ns(name, Namespace::Type) {
            if self.is_decl_private(uri, decl) {
                continue;
            }
            if let Some(item) = self.item_id_for(uri, name)
                && decl_registry.lookup(item).is_some()
            {
                return Some(item);
            }
        }
        None
    }

    /// `Symbol`-keyed location index. Caller must have
    /// already interned `name_sym` through `self.symbols`. `ns` is the
    /// namespace of the decl being registered — letting downstream
    /// lookups segregate type-position from value-position hits.
    fn record_decl_location(
        &mut self,
        name_sym: Symbol,
        uri: &Uri,
        decl_id: Idx<Decl>,
        ns: Namespace,
    ) {
        let entry = self.decl_locations.entry(name_sym).or_default();
        if !entry.iter().any(|d| &d.uri == uri && d.id == decl_id) {
            entry.push(DeclLocation {
                uri: uri.clone(),
                id: decl_id,
                ns,
            });
        }
    }

    /// Cross-module decl lookup: every `(Uri, Idx<Decl>, Namespace)` triple
    /// known under this name. Empty slice when the name is unknown.
    pub fn locate_decl(&self, name: Symbol) -> &[DeclLocation] {
        self.decl_locations
            .get(&name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Same as [`Self::locate_decl`] but filtered to a single namespace.
    /// Used by the resolver (`record_use` / `bind_qualified_type_leaf`)
    /// and `resolve_decl_handle` to avoid cross-namespace false
    /// matches (e.g. `type geo` vs `fn geo()`).
    pub fn locate_decl_in_ns(
        &self,
        name: Symbol,
        ns: Namespace,
    ) -> impl Iterator<Item = (&Uri, Idx<Decl>)> {
        self.locate_decl(name)
            .iter()
            .filter(move |d| d.ns == ns)
            .map(|d| (&d.uri, d.id))
    }

    /// `true` iff `name` resolves against any name the project knows:
    /// a registered type / enum, a function, or a variable.
    /// Resolver uses this as the post-local-scope fallback.
    pub fn has_name(&self, name: Symbol) -> bool {
        self.type_names.contains(&name)
            || self.fn_names.contains(&name)
            || self.var_names.contains(&name)
    }
}

/// Extract the module name from a URI (filename without
/// `.gcl`). Mirrors [`Document::name`](greycat_analyzer_core::Document)
/// without the borrow on a manager.
pub fn module_name_from_uri(uri: &Uri) -> Option<&str> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    let last = stripped.rsplit(['/', '\\']).next()?;
    let stem = last.strip_suffix(".gcl").unwrap_or(last);
    if stem.is_empty() { None } else { Some(stem) }
}

/// Names the analyzer treats as known without an `.gcl` declaration in
/// scope: the GreyCat primitives. Seeded straight into the project's
/// [`SymbolTable`] + `type_names` set so the resolver's "is this a
/// known type?" fallback and `lower_type_ref*`'s primitive minting
/// paths both recognise them. No `TypeArena` writes — `seed_builtins`
/// (in `project.rs`) is the canonical seed for primitive TypeIds.
///
/// Runtime-implemented types (collections, node tags,
/// `function` / `type` / `field` markers) are no longer seeded here:
/// they all have `native type` decls in `lib/std/core.gcl` and land
/// in `type_names` through the normal stdlib ingest path.
fn seed_builtin_names(symbols: &mut SymbolTable, type_names: &mut FxHashSet<Symbol>) {
    for &name in &[
        "bool", "int", "float", "char", "String", "time", "duration", "geo", "any", "null",
    ] {
        let sym = symbols.intern(name);
        type_names.insert(sym);
    }
}

/// Read `@iterable` / `@deref` / `@primitive` annotations on a
/// type decl into a [`TypeFlags`] record.
fn derive_type_flags(symbols: &SymbolTable, annotations: &[Annotation]) -> TypeFlags {
    let mut flags = TypeFlags::default();
    for ann in annotations {
        match &symbols[ann.name.symbol] {
            "iterable" => flags.iterable = true,
            "primitive" => flags.primitive = true,
            "deref" => flags.deref = ann.first_string_arg(),
            _ => {}
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;
    use std::str::FromStr;

    fn lower(symbols: &SymbolTable, src: &str) -> Hir {
        let tree = parse(src);
        lower_module(src, symbols, "stdmod", "std", tree.root_node())
    }

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).unwrap()
    }

    /// Spin up the four pieces of state a real `ProjectAnalysis`
    /// threads through `ingest` — the shared `TypeArena`, the
    /// decl-handle interner, the well-known-slot table, and the
    /// index itself. Returned by-value so each test owns an
    /// independent copy.
    fn fresh_index() -> (TypeArena, DeclRegistry, WellKnown, ProjectIndex) {
        let mut arena = TypeArena::new();
        let decl_registry = DeclRegistry::default();
        let well_known = WellKnown::default();
        let idx = ProjectIndex::new(&mut arena);
        (arena, decl_registry, well_known, idx)
    }

    #[test]
    fn ingest_registers_type_decls() {
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            r#"
type Person {
    name: String;
    age: int;
}

type Company {
    people: Array<Person>;
}
"#,
        );
        idx.ingest(
            &uri("/proj/people.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        assert_eq!(idx.modules_ingested, 1);
        assert!(idx.has_name(idx.symbols.lookup("Person").unwrap()));
        assert!(idx.has_name(idx.symbols.lookup("Company").unwrap()));
    }

    #[test]
    fn ingest_registers_enum_decls() {
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(&idx.symbols, "enum Color { Red, Green, Blue }\n");
        idx.ingest(
            &uri("/proj/color.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        let color_id = idx
            .item_id_for(
                &uri("/proj/color.gcl"),
                idx.symbols.lookup("Color").expect("Color interned"),
            )
            .expect("Color item id");
        let id = idx
            .enum_types
            .get(&color_id)
            .copied()
            .expect("Color registered");
        let ty = arena.get(id);
        let TypeKind::Enum { variants, .. } = &ty.kind else {
            panic!("expected enum, got {ty:?}");
        };
        let variant_names: Vec<&str> = variants.iter().map(|s| &idx.symbols[*s]).collect();
        assert_eq!(variant_names, ["Red", "Green", "Blue"]);
    }

    #[test]
    fn ingest_is_idempotent_on_repeated_calls() {
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(&idx.symbols, "type T {}\n");
        let u = uri("/proj/t.gcl");
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        let len_after_first = arena.len();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        assert_eq!(arena.len(), len_after_first, "duplicate type registrations");
        assert_eq!(idx.modules_ingested, 2);
        // decl_locations is also idempotent — the same (uri, decl_id)
        // pair shouldn't be appended twice.
        assert_eq!(idx.locate_decl(idx.symbols.lookup("T").unwrap()).len(), 1);
    }

    #[test]
    fn locate_decl_records_uri_and_decl_id() {
        // Acceptance for P11.1: querying the index for a declared type
        // returns the URI of the module that introduced it and a
        // matching `Idx<Decl>`. Synthetic stand-in for `Foo` in
        // `lib/std/runtime.gcl` so the test doesn't depend on `greycat
        // install` having been run.
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(&idx.symbols, "private type Foo {}\n");
        let uri = uri("/proj/lib/std/runtime.gcl");
        idx.ingest(&uri, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let hits = idx.locate_decl(idx.symbols.lookup("Foo").unwrap());
        assert_eq!(hits.len(), 1, "exactly one Foo decl across project");
        let d = &hits[0];
        assert_eq!(d.uri, uri);
        assert!(matches!(&hir.decls[d.id], Decl::Type(_)));
        assert_eq!(d.ns, Namespace::Type);
    }

    // P19.9
    /// Every name `ingest` records into the project
    /// index also lands in the [`SymbolTable`]. This anchors the
    /// invariant the new `&str` accessors (`type_members_for`,
    /// `fn_signature_for`, etc.) rely on: a successful `ingest` of
    /// "Foo" → `idx.symbol("Foo")` answers `Some(_)` and the
    /// returned [`Symbol`] keys every map that holds a "Foo" entry.
    #[test]
    fn ingest_interns_names_into_symbol_table() {
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            r#"
type Bag {
    weight: int;
    fn lift(): int;
}

enum Color { Red, Green }

fn helper(): int { return 1; }
"#,
        );
        idx.ingest(
            &uri("/proj/m.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );

        // Each top-level decl name is interned.
        for n in ["Bag", "Color", "helper"] {
            let sym = idx
                .symbol(n)
                .unwrap_or_else(|| panic!("{n} not interned after ingest"));
            assert_eq!(idx.symbols.resolve(&sym), n);
        }

        // Direct ItemId-keyed field access — `m.gcl` → module `m`.
        let bag_id = idx
            .item_id_for(&uri("/proj/m.gcl"), idx.symbol("Bag").unwrap())
            .expect("Bag item id");
        let bag = idx.type_members.get(&bag_id).expect("Bag in type_members");
        let weight_sym = idx.symbol("weight").expect("weight interned via ingest");
        let lift_sym = idx.symbol("lift").expect("lift interned via ingest");
        assert!(bag.attrs.contains_key(&weight_sym));
        assert!(bag.methods.contains_key(&lift_sym));
    }

    #[test]
    fn is_subtype_of_decl_resolves_handles_then_delegates_to_name_keyed() {
        // Inheritance graph: `Cat extends Animal`. `is_subtype_of_decl`
        // takes handles, looks each up in the arena's decl-names table,
        // and asks the existing name-keyed walker. Equal handles
        // short-circuit without arena access; missing handles return
        // false.
        let (mut idx_arena, mut idx_decl_registry, mut idx_well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            "type Animal { name: String; }\n\
             type Cat extends Animal { whiskers: int; }\n",
        );
        let u = uri("/proj/m.gcl");
        idx.ingest(
            &u,
            &hir,
            &mut idx_arena,
            &mut idx_decl_registry,
            &mut idx_well_known,
        );

        // Mint handles into a fresh `DeclRegistry` / `TypeArena` pair
        // so the test exercises `is_subtype_of_decl` against a
        // freshly-built name table — independent of whatever ingest
        // chose to mint into `idx_arena`.
        let mut registry = DeclRegistry::new();
        let mut arena = TypeArena::new();
        let module = hir.module.as_ref().unwrap();
        let mut animal = None;
        let mut cat = None;
        for decl_id in &module.decls {
            if let Decl::Type(td) = &hir.decls[*decl_id] {
                let name_sym = hir.idents[td.name].symbol;
                let item = idx.item_id_for(&u, name_sym).unwrap();
                registry.record(item, *decl_id);
                arena.alloc_type(item);
                match &idx.symbols[name_sym] {
                    "Animal" => animal = Some(item),
                    "Cat" => cat = Some(item),
                    _ => {}
                }
            }
        }
        let animal = animal.unwrap();
        let cat = cat.unwrap();

        assert!(idx.is_subtype_of_decl(cat, animal));
        assert!(!idx.is_subtype_of_decl(animal, cat));
        // Reflexivity short-circuits regardless of registry membership.
        assert!(idx.is_subtype_of_decl(animal, animal));

        // A handle whose name was never registered in the arena returns
        // false (no panic).
        let dangling_name = idx.symbols.intern("__dangling__");
        let dangling_uri = uri("/other.gcl");
        let dangling = idx.item_id_for(&dangling_uri, dangling_name).unwrap();
        registry.record(dangling, Idx::<Decl>::from_raw(99u32));
        assert!(!idx.is_subtype_of_decl(dangling, animal));
    }

    #[test]
    fn locate_decl_keeps_collisions_across_modules() {
        // Same name in two modules should produce two entries — P11.2
        // disambiguates at the use site via the importer's lib/include
        // closure, but the table itself keeps every hit.
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir_a = lower(&idx.symbols, "type Helper {}\n");
        let hir_b = lower(&idx.symbols, "type Helper {}\n");
        idx.ingest(
            &uri("/proj/a.gcl"),
            &hir_a,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        idx.ingest(
            &uri("/proj/b.gcl"),
            &hir_b,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        let hits = idx.locate_decl(idx.symbols.lookup("Helper").unwrap());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].uri, uri("/proj/a.gcl"));
        assert_eq!(hits[1].uri, uri("/proj/b.gcl"));
    }

    #[test]
    fn ingest_captures_expose_rename_into_exposed_map() {
        // P13.4: `@expose("renamed")` keys into ProjectIndex::exposed by
        // the renamed string; bare `@expose` keys by the decl's local
        // name.
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            r#"
@expose("public_alpha")
fn alpha() {}

@expose
fn beta() {}

@library("std", "1")
fn ignored() {}
"#,
        );
        let u = uri("/proj/api.gcl");
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let alpha_sym = idx.symbol("public_alpha").expect("public_alpha interned");
        let alpha_hits = idx.exposed.get(&alpha_sym).expect("public_alpha");
        assert_eq!(alpha_hits.len(), 1);
        assert_eq!(
            alpha_hits[0].rename.map(|s| &idx.symbols[s]),
            Some("public_alpha")
        );
        assert_eq!(&idx.symbols[alpha_hits[0].local_name], "alpha");

        let beta_sym = idx.symbol("beta").expect("beta interned");
        let beta_hits = idx.exposed.get(&beta_sym).expect("beta");
        assert_eq!(beta_hits.len(), 1);
        assert_eq!(beta_hits[0].rename, None);

        assert!(
            idx.symbol("ignored")
                .is_none_or(|s| !idx.exposed.contains_key(&s)),
            "@library annotation shouldn't add to exposed map",
        );
    }

    #[test]
    fn ingest_captures_type_flags_from_annotations() {
        // P13.5: @iterable / @deref / @primitive annotations on a type
        // decl populate ProjectIndex.type_flags.
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            r#"
@iterable
@deref("resolve")
type Bag {}

@primitive
type Marker {}

type Plain {}
"#,
        );
        let u = uri("/proj/m.gcl");
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let item = |name: &str| {
            idx.item_id_for(&u, idx.symbols.lookup(name).unwrap())
                .unwrap()
        };
        let bag = idx.type_flags.get(&item("Bag")).expect("Bag flags");
        assert!(bag.iterable);
        assert_eq!(bag.deref.map(|s| &idx.symbols[s]), Some("resolve"));
        assert!(!bag.primitive);

        let marker = idx.type_flags.get(&item("Marker")).expect("Marker flags");
        assert!(marker.primitive);
        assert!(!marker.iterable);

        // Plain has no annotations — kept out of the map.
        assert!(!idx.type_flags.contains_key(&item("Plain")));
    }

    #[test]
    fn ingest_captures_permission_pragmas_per_module() {
        // P13.6: `@permission("name")` pragma populates
        // ProjectIndex::module_permissions[uri].
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            "@permission(\"admin\");\n@permission(\"user\");\nfn handler() {}\n",
        );
        let u = uri("/proj/api.gcl");
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let perms = idx.module_permissions.get(&u).expect("permissions tracked");
        let admin_sym = idx.symbol("admin").expect("admin interned");
        let user_sym = idx.symbol("user").expect("user interned");
        assert!(perms.contains(&admin_sym));
        assert!(perms.contains(&user_sym));
        assert_eq!(perms.len(), 2);
    }

    #[test]
    fn locate_decl_records_fns_and_top_vars() {
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        let hir = lower(
            &idx.symbols,
            r#"
fn helper(): int { return 1; }
var TOP: int = 1;
"#,
        );
        let u = uri("/proj/m.gcl");
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        assert_eq!(
            idx.locate_decl(idx.symbols.lookup("helper").unwrap()).len(),
            1
        );
        assert_eq!(idx.locate_decl(idx.symbols.lookup("TOP").unwrap()).len(), 1);
        assert!(
            idx.symbols
                .lookup("missing")
                .is_none_or(|sym| idx.locate_decl(sym).is_empty())
        );
    }
}
