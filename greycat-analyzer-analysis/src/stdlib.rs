//! Stdlib ingestion.
//!
//! Loads `lib/std/*.gcl` as ordinary HIR modules and registers their
//! declared types and native-bound function signatures into shared
//! [`TypeArena`] / [`TypeRegistry`] / [`NativeRegistry`] structures so
//! the analyzer can resolve `int`, `String`, `Array`, `node`, etc.
//! against real declarations rather than the stub `BUILTIN_TYPES`
//! allowlist the resolver currently pre-seeds.
//!
//! Decision F (ROADMAP §3): runtime-implemented (`native`) functions
//! get a small Rust metadata table — signatures only, no bodies. Their
//! .gcl source captures the signature; this module collects them so
//! call-site type checking works even though there's no body to walk.

use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{
    Primitive, Symbol, SymbolTable, Type, TypeArena, TypeDeclId, TypeId, TypeKind,
};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Annotation, Decl, FnDecl, TypeAttr, TypeRef as HirTypeRef};

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

/// Cross-module registry of native-bound function signatures. Keyed by
/// canonical name (`<lib>::<module>::<fn>` once we wire fully-qualified
/// resolution; just `<fn>` for now until  multi-module work).
///
// P19.9
/// Keys are project-wide [`Symbol`]s. Lookup helpers that
/// take `&str` translate via the project's [`SymbolTable`] (held on
/// [`ProjectIndex`]); see [`ProjectIndex::native_for`].
#[derive(Debug, Default)]
pub struct NativeRegistry {
    pub signatures: FxHashMap<Symbol, NativeSignature>,
}

#[derive(Debug, Clone)]
pub struct NativeSignature {
    pub params: Vec<TypeId>,
    pub return_ty: TypeId,
}

impl NativeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // P19.9
    /// Register by an already-interned [`Symbol`]. Callers
    /// that hold a `&str` should go through [`ProjectIndex::ingest`]
    /// (which interns into `index.symbols` and forwards here).
    pub fn register(&mut self, sym: Symbol, sig: NativeSignature) {
        self.signatures.insert(sym, sig);
    }

    // P19.9
    /// Lookup by a previously-interned [`Symbol`].
    /// `&str` callers should use [`ProjectIndex::native_for`].
    pub fn lookup_sym(&self, sym: Symbol) -> Option<&NativeSignature> {
        self.signatures.get(&sym)
    }
}

/// Cross-module project context: name tables / structure indices /
/// native-fn signatures that survive across module ingestion. Distinct
/// from [`crate::analyzer::AnalysisResult`], which is per-module.
///
/// The shared `TypeArena` does NOT live here — it's owned by
/// [`crate::project::ProjectAnalysis`] and threaded through `ingest`
/// at construction time so type/enum allocations land in the *one*
/// arena every downstream stage reads. Earlier revisions kept a
/// second `TypeArena` on `ProjectIndex`, but its TypeIds were
/// orphans (no consumer ever read them) and the duplication forced
/// `lower_module_signatures` to re-allocate every enum, which made
/// `enum_types` lag the actual signature pass and surface as a
/// self-named `T not assignable to T` regression class.
///
// P19.9
/// Every project-wide map keys on [`Symbol`] instead of
/// `String`. The names live once in [`Self::symbols`]; map keys are
/// 32-bit handles. Public lookup helpers ([`Self::has_name`],
/// [`Self::locate_decl`], [`Self::type_members_for`],
/// [`Self::fn_signature_for`], [`Self::enum_type_for`],
/// [`Self::native_for`], [`Self::type_flags_for`], …) keep the
/// historical `&str` API surface — they translate via `symbols`
/// internally.
#[derive(Debug, Default)]
pub struct ProjectIndex {
    // P19.9
    /// Project-wide string interner. Owns the canonical
    /// storage for every type / fn / attr / method / enum-variant /
    /// global / module name the analyzer looks up across modules.
    pub symbols: SymbolTable,
    /// Set of [`Symbol`]s the analyzer recognises as a *type* name:
    /// every primitive plus every `type` / `enum` / `native type`
    /// decl ingested from a `.gcl` file (the runtime-implemented
    /// types all have `native type` decls in `lib/std/core.gcl` and
    /// land here through the normal ingest path). Drives
    /// [`Self::has_name`] and the signature-cache fingerprint
    /// ([`project_name_set_hash`]).
    pub type_names: FxHashSet<Symbol>,
    pub natives: NativeRegistry,
    /// Top-level value-position names from every ingested module —
    /// non-native `fn` declarations, top-level `var` declarations.
    /// Lets the resolver answer "is this name known anywhere in the
    /// project?" without needing the cross-module decl pointer
    /// (a later deliverable).
    pub values: FxHashSet<Symbol>,
    // P38.1
    /// Names of non-native top-level `fn` declarations from every
    /// ingested module. Subset of `values` — `values` also contains
    /// top-level `var` names with no way to distinguish them from
    /// non-native fns by membership alone. Lets the analyzer's
    /// `Definition::ProjectDecl` value-typing arm route bare fn
    /// idents to `function_ty()` instead of falling through to
    /// `type_ty()` via `has_name`. Natives stay in `fn_signatures`
    /// (which carries the lowered signature) — this set is for the
    /// "decl exists, type as `function`" question, nothing more.
    pub non_native_fn_names: FxHashSet<Symbol>,
    // P38.4
    /// `(Uri, Idx<Decl>)` pairs whose decl carries the `private`
    /// modifier. Mirrors the entries in [`Self::decl_locations`] —
    /// every private decl that goes through `record_decl_location`
    /// also gets recorded here so the resolver's bare-name lookup
    /// can filter cross-module candidates by visibility (private
    /// decls don't participate in bare cross-module resolution; they
    /// stay reachable only via FQN). Empty by default; populated by
    /// [`Self::ingest`].
    pub private_locations: FxHashSet<(Uri, Idx<Decl>)>,
    /// Cross-module decl table: name → every `(Uri, Idx<Decl>)`
    /// pair that introduces a top-level decl with this name across the
    /// project. Collisions are kept; disambiguation happens at the use
    /// site via the importing module's lib/include closure.
    /// Pragma decls have no name and are excluded.
    pub decl_locations: FxHashMap<Symbol, Vec<(Uri, Idx<Decl>)>>,
    // P13.4
    /// Runtime-exposed names. Keyed by the rename string of
    /// `@expose("renamed")` (or the decl's own name when `@expose` has
    /// no arg) → every site that exposed under that key. Lets lints /
    /// capabilities ask "is this name part of the runtime API?".
    pub exposed: FxHashMap<Symbol, Vec<ExposureSite>>,
    // P13.5
    /// Per-type flag bits drawn from `@iterable` / `@deref` /
    /// `@primitive` annotations on a `type` decl. Keyed by the
    /// declared type name (`Array`, `nodeTime`, …).
    pub type_flags: FxHashMap<Symbol, TypeFlags>,
    // P13.6
    /// Per-module `@permission("name")` pragmas. Lets later
    /// chunks light up "is this module allowed to call X?" checks the
    /// TS reference declarator threads through `mod.permissions`.
    pub module_permissions: FxHashMap<Uri, FxHashSet<Symbol>>,
    // P15.x
    /// Module-name → URI. Populated from each ingested doc's
    /// filename stem (i.e. `Document::name()`). Lets the resolver
    /// recognize `runtime` in `runtime::Identity::create` as a known
    /// module name (rather than flagging it as unresolved), and lets
    /// pass 3.5 infer types for `module::Decl` static expressions.
    pub module_names: FxHashMap<Symbol, Uri>,
    // P21
    /// Pre-computed cross-module structure index. Keyed by type
    /// name (as it appears in source). For each type, records the
    /// home module URI and a (property name → HIR `Idx`) lookup for
    /// both attrs and methods. Built incrementally by [`Self::ingest`].
    ///
    /// The first ingested decl for a given name wins (matches the
    /// existing `decl_locations` collision semantics — disambiguation
    /// across libs happens at the use site via the importing module's
    /// lib/include closure. Pass 3
    /// (`resolve_cross_module_members`) used to drain a per-module
    /// `deferred_member_uses` against `decl_locations` after the
    /// per-module analyzer pass; with this index the analyzer's
    /// `resolve_member` resolves cross-module hits inline at body-walk
    /// time, removing the deferral.
    pub type_members: FxHashMap<Symbol, TypeMembers>,
    // P23
    /// Pre-lowered top-level fn signatures, keyed by fn
    /// name. First-decl-wins, matching `type_members` collision
    /// semantics. `home_uri` lets the analyzer's call-typing path
    /// disambiguate the right module when needed; `return_ty` is
    /// already minted into the shared arena, so the analyzer applies
    /// `arena.substitute` at the call site for generic fns. Built
    /// by `ProjectAnalysis::stage_lower_signatures` after every
    /// module is loaded but before any body walks.
    pub fn_signatures: FxHashMap<Symbol, FnSignature>,
    // P23
    /// Enum types pre-registered in the shared project
    /// arena, keyed by enum name. Lets the analyzer's
    /// `QualifiedStatic` value-position typing recognise
    /// `other_module::Foo::a` as the enum `Foo` (not `any`).
    pub enum_types: FxHashMap<Symbol, TypeId>,
    // P19.10
    /// Pre-lowered top-level `var` declared types,
    /// keyed by var name. First-decl-wins (same collision rule as
    /// the rest of the per-name indexes). Built by
    /// `ProjectAnalysis::stage_lower_signatures`. Lets the analyzer
    /// type a bare cross-module reference (`Definition::ProjectDecl`
    /// pointing at a `Decl::Var`) inline at body-walk time
    /// without this, `for (k, v in foreign_groups)` over a
    /// `nodeIndex<String, node<Group>>` declared in another module
    /// would type the iterable as `type` and bind `v` to `any`.
    pub var_types: FxHashMap<Symbol, TypeId>,
    // P19.16
    /// Runtime-implemented value-position globals
    /// (`Infinity`, `NaN`, `-Infinity`) and their declared type.
    /// These have no `.gcl` declaration but the runtime exposes them
    /// at well-known names. Seeded once in `seed_builtin_names` and
    /// consumed by the analyzer's `Expr::Ident` arm when the
    /// resolver returns `Definition::Project`. Without this entry
    /// the names would resolve through `has_name` (we register them
    /// in `values` too) but the body-walker would type them as
    /// `any`, masking float/int dispatch downstream.
    pub runtime_globals: FxHashMap<Symbol, TypeId>,
    /// Total number of modules ingested. Useful for "did stdlib actually
    /// load?" smoke checks at the LSP boundary.
    pub modules_ingested: usize,
}

// P23
/// Top-level fn signature record. `return_ty` is the
/// pre-lowered return `TypeId` in the shared project arena; it may
/// be `GenericParam(T, owner=fn)` for generic fns. The analyzer's
/// `try_member_call_typing` consults this for cross-module Ident
/// callees and `QualifiedStatic` `module::fn` shapes.
#[derive(Debug, Clone)]
pub struct FnSignature {
    pub home_uri: Uri,
    pub return_ty: TypeId,
    // P19.9
    /// Interned generic param names. Resolve back to text
    /// via the owning [`ProjectIndex::symbols`].
    pub generics: Vec<Symbol>,
    // P19.15
    /// Pre-lowered parameter types in declared order.
    /// Lets the analyzer's generic-call inference (`try_generic_call_inference`)
    /// run for cross-module `Definition::ProjectDecl` callees too
    /// without these the inference path could only fire for
    /// in-module `Definition::Decl` because the foreign HIR isn't
    /// reachable from the body walker. Empty for fns declared with
    /// no params.
    pub params: Vec<TypeId>,
}

// P21
/// Per-type cross-module member index. `home_uri` names the
/// module that declared the type so the analyzer / staged orchestrator
/// can fish the right `Hir` out of `ProjectAnalysis::modules`.
///
/// **Signature-lowering extension:** `generics`, `attr_types`, and `method_returns`
/// hold the *project-wide-lowered* signature data. Built by
/// `ProjectAnalysis::stage_lower_signatures` after every module is
/// loaded but before any body walks. With these, the analyzer can
/// type a foreign `recv.attr` / `recv.method()` inline by looking up
/// the relevant TypeId in the shared arena and applying
/// `arena.substitute` against the receiver's instantiation — no
/// post-pass round-trip via `TypeShape`.
#[derive(Debug, Clone)]
pub struct TypeMembers {
    pub home_uri: Uri,
    // P19.9
    /// Attr name → HIR index. Symbol-keyed; resolve to
    /// text via [`ProjectIndex::symbols`].
    pub attrs: FxHashMap<Symbol, Idx<TypeAttr>>,
    // P19.9
    /// Method name → HIR index. Symbol-keyed.
    pub methods: FxHashMap<Symbol, Idx<Decl>>,
    // P22
    /// Ordered list of generic parameter names declared on the
    /// type (`type Map<K, V> {}` → `[Sym("K"), Sym("V")]`). Empty for
    /// non-generic types. Used by the analyzer to build a
    /// `name → TypeId` substitution map from the receiver's
    /// instantiation args at member-access / call sites.
    pub generics: Vec<Symbol>,
    // P22
    /// Pre-lowered attr declared types, keyed by attr-name
    /// [`Symbol`]. `TypeId`s reference the shared project arena
    /// ([`crate::project::ProjectAnalysis::arena`]). For generic
    /// types, attr TypeIds may reference `GenericParam(T, owner=this)`
    /// — call-site substitution is the consumer's job.
    pub attr_types: FxHashMap<Symbol, TypeId>,
    // P22
    /// Pre-lowered method declared return types. Same arena +
    /// substitution semantics as `attr_types`. Methods without an
    /// explicit return type are absent (the analyzer's call-typing
    /// falls through to the existing inference path).
    pub method_returns: FxHashMap<Symbol, TypeId>,
    // P19.13
    /// Names of attrs declared with the `static`
    /// modifier (`type Foo { static path: String = "..." }`).
    /// Lets the analyzer's `Expr::Static` value-typing
    /// distinguish `Foo::path` (which is the value, typed as
    /// `String`) from a non-static `Foo::path` reference (which is
    /// a runtime `field` handle). Empty for types with no static
    /// attrs.
    pub static_attrs: FxHashSet<Symbol>,
    /// Names of methods declared with the `static` modifier. Lets
    /// `resolve_member` filter them out of instance access — the
    /// runtime resolves `this.from` to an inherited `from: time` attr
    /// even when a `static fn from(...)` is declared on the same type.
    pub static_methods: FxHashSet<Symbol>,
    /// Names of methods declared with the `abstract` modifier.
    /// Captured at ingest so the LSP `textDocument/declaration`
    /// handler can walk the supertype chain looking for the abstract
    /// ancestor of a concrete override without needing to fetch each
    /// foreign module's HIR.
    pub abstract_methods: FxHashSet<Symbol>,
    // P19.14
    /// Direct supertype name (the `Super` in
    /// `type Sub extends Super`). Drives inheritance: member
    /// lookup walks `supertype` chains to find inherited
    /// attrs / methods, and assignability recognises
    /// `Named(Sub)` → `Named(Super)` (and `node<Sub>` →
    /// `node<Super>`) when `Sub` is a descendant of `Super`.
    /// `None` for types without an explicit `extends` clause.
    pub supertype: Option<Symbol>,
    /// Pre-resolved deref-target TypeId for types carrying a
    /// `@deref("methodName")` annotation. Captured during signature
    /// lowering: the `@deref` annotation names a method, and this
    /// field caches the method's pre-lowered return TypeId (still in
    /// the abstract `GenericParam(T, …)` form — call-site
    /// substitution applies the receiver's instantiation). Lets
    /// `arrow_deref_receiver` answer `*n` / `n->m()` typing with a
    /// single field read instead of a name lookup + chain walk per
    /// access. `None` when the type has no `@deref` annotation.
    pub deref_return_ty: Option<TypeId>,
}

impl TypeMembers {
    // P19.9
    /// `&str` lookup of an attr's HIR index. Returns
    /// `None` if `name` isn't interned in `symbols` or if no attr
    /// of that name exists on this type.
    pub fn attr_id(&self, symbols: &SymbolTable, name: &str) -> Option<Idx<TypeAttr>> {
        symbols
            .lookup(name)
            .and_then(|s| self.attrs.get(&s))
            .copied()
    }
    // P19.9
    /// `&str` lookup of a method's HIR index.
    pub fn method_id(&self, symbols: &SymbolTable, name: &str) -> Option<Idx<Decl>> {
        symbols
            .lookup(name)
            .and_then(|s| self.methods.get(&s))
            .copied()
    }
    // P19.9
    /// `&str` lookup of an attr's pre-lowered type.
    pub fn attr_ty(&self, symbols: &SymbolTable, name: &str) -> Option<TypeId> {
        symbols
            .lookup(name)
            .and_then(|s| self.attr_types.get(&s))
            .copied()
    }
    // P19.9
    /// `&str` lookup of a method's pre-lowered return type.
    pub fn method_return(&self, symbols: &SymbolTable, name: &str) -> Option<TypeId> {
        symbols
            .lookup(name)
            .and_then(|s| self.method_returns.get(&s))
            .copied()
    }
}

// P13.5
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
    // P25.6
    pub deref: Option<SmolStr>,
    pub primitive: bool,
}

// P13.4
/// A single `@expose`-annotated decl, recorded for the
/// runtime-API surface. `local_name` is the source-level name in the
/// declaring module; `rename` is what `@expose("renamed")` gave it
/// (or `None` when `@expose` was used bare).
#[derive(Debug, Clone)]
pub struct ExposureSite {
    pub uri: Uri,
    pub decl: Idx<Decl>,
    // P25.6
    pub local_name: SmolStr,
    // P25.6
    pub rename: Option<SmolStr>,
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

    // P19.9
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
        // **P19.16** — runtime-exposed value-position globals
        // (`Infinity`, `NaN`). Registered here (not in
        // `seed_builtin_names`) because they're values, not types,
        // and they need a typed `TypeId` the body walker can consume.
        for (name, prim) in BUILTIN_RUNTIME_GLOBALS {
            let sym = idx.symbols.intern(name);
            let ty = arena.primitive(*prim);
            idx.runtime_globals.insert(sym, ty);
            idx.values.insert(sym);
        }
        idx
    }

    // P19.9
    /// Read-only `&str` → [`Symbol`] lookup. Returns
    /// `None` if `name` was never interned. Hot lookup paths use
    /// this to avoid mutating the symbol table from a `&self`
    /// borrow.
    pub fn symbol(&self, name: &str) -> Option<Symbol> {
        self.symbols.lookup(name)
    }

    // P19.9
    /// `&str`-keyed lookup helpers preserved for
    /// callers that don't (yet) hold a [`Symbol`]. Each does one
    /// `symbols.lookup` then a Symbol-keyed map probe.
    pub fn type_members_for(&self, name: &str) -> Option<&TypeMembers> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.type_members.get(&s))
    }
    pub fn fn_signature_for(&self, name: &str) -> Option<&FnSignature> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.fn_signatures.get(&s))
    }
    pub fn enum_type_for(&self, name: &str) -> Option<TypeId> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.enum_types.get(&s))
            .copied()
    }
    // P19.10
    /// `&str` lookup of a top-level var's declared type.
    pub fn var_type_for(&self, name: &str) -> Option<TypeId> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.var_types.get(&s))
            .copied()
    }
    // P19.16
    /// `&str` lookup of a runtime-exposed value
    /// global's type (e.g. `Infinity` → `float`). See
    /// [`BUILTIN_RUNTIME_GLOBALS`].
    pub fn runtime_global_for(&self, name: &str) -> Option<TypeId> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.runtime_globals.get(&s))
            .copied()
    }

    // P19.14
    /// Walk the supertype chain starting at `type_name`,
    /// returning the first `TypeMembers` entry that contains the
    /// member matched by `pred`. Used to find inherited attrs /
    /// methods (`pvInstallation->timezone` resolves through
    /// `PVInstallation extends PVEntity`'s `timezone: TimeZone`).
    /// Number of types in `type_name`'s supertype chain, counting the
    /// type itself. Returns 0 when `type_name` is unknown. Stops
    /// counting at [`MAX_SUPERTYPE_CHAIN_DEPTH`] + 1 — the caller only
    /// needs to distinguish "within limit" from "exceeds limit".
    pub fn supertype_chain_length(&self, type_name: &str) -> usize {
        let Some(mut cur) = self.symbols.lookup(type_name) else {
            return 0;
        };
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
    fn walk_member_chain<P>(&self, type_name: &str, mut pred: P) -> Option<&TypeMembers>
    where
        P: FnMut(&TypeMembers) -> bool,
    {
        let mut cur = self.symbols.lookup(type_name)?;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let members = self.type_members.get(&cur)?;
            if pred(members) {
                return Some(members);
            }
            cur = members.supertype?;
        }
        None
    }

    // P19.14
    /// `&str` lookup of an attr's HIR index, walking
    /// the supertype chain. Returns the `(home_uri, attr_id)` of
    /// the type that owns the attr (which may be the type itself
    /// or a parent), so cross-module hover / goto-def points at
    /// the right module.
    pub fn type_attr_id_chain(
        &self,
        type_name: &str,
        attr_name: &str,
    ) -> Option<(Uri, Idx<TypeAttr>)> {
        let attr_sym = self.symbols.lookup(attr_name)?;
        let members = self.walk_member_chain(type_name, |m| m.attrs.contains_key(&attr_sym))?;
        members
            .attrs
            .get(&attr_sym)
            .map(|id| (members.home_uri.clone(), *id))
    }

    // P19.14
    /// `&str` lookup of a method's HIR index, walking
    /// the supertype chain.
    pub fn type_method_id_chain(
        &self,
        type_name: &str,
        method_name: &str,
    ) -> Option<(Uri, Idx<Decl>)> {
        let method_sym = self.symbols.lookup(method_name)?;
        let members = self.walk_member_chain(type_name, |m| m.methods.contains_key(&method_sym))?;
        members
            .methods
            .get(&method_sym)
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
        type_name: &str,
        method_name: &str,
    ) -> Option<(Uri, Idx<Decl>)> {
        let method_sym = self.symbols.lookup(method_name)?;
        let members = self.walk_member_chain(type_name, |m| {
            m.methods.contains_key(&method_sym) && !m.static_methods.contains(&method_sym)
        })?;
        members
            .methods
            .get(&method_sym)
            .map(|id| (members.home_uri.clone(), *id))
    }

    /// Walk the *strict* supertype chain of `type_name` (skipping the
    /// type itself) looking for an ancestor that declares
    /// `method_name` with the `abstract` modifier. Returns
    /// `(home_uri, Idx<Decl>)` of the abstract declaration, or `None`
    /// if no abstract ancestor exists. Powers
    /// `textDocument/declaration`: the inverse of
    /// `textDocument/implementation`.
    pub fn find_abstract_ancestor_method(
        &self,
        type_name: &str,
        method_name: &str,
    ) -> Option<(Uri, Idx<Decl>)> {
        let method_sym = self.symbols.lookup(method_name)?;
        let start = self.symbols.lookup(type_name)?;
        let start_members = self.type_members.get(&start)?;
        let mut cur = start_members.supertype?;
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let members = self.type_members.get(&cur)?;
            if members.abstract_methods.contains(&method_sym)
                && let Some(decl_id) = members.methods.get(&method_sym)
            {
                return Some((members.home_uri.clone(), *decl_id));
            }
            cur = members.supertype?;
        }
        None
    }

    // P19.14
    /// Pre-lowered attr type, walking the supertype
    /// chain. The `TypeId` lives in the project arena and may
    /// reference `GenericParam(T, owner=parent_type)` if the attr
    /// is declared on a generic parent.
    pub fn type_attr_ty_chain(&self, type_name: &str, attr_name: &str) -> Option<TypeId> {
        let attr_sym = self.symbols.lookup(attr_name)?;
        let members =
            self.walk_member_chain(type_name, |m| m.attr_types.contains_key(&attr_sym))?;
        members.attr_types.get(&attr_sym).copied()
    }

    // P19.14
    /// Pre-lowered method return type, walking the
    /// supertype chain.
    pub fn type_method_return_chain(&self, type_name: &str, method_name: &str) -> Option<TypeId> {
        let method_sym = self.symbols.lookup(method_name)?;
        let members =
            self.walk_member_chain(type_name, |m| m.method_returns.contains_key(&method_sym))?;
        members.method_returns.get(&method_sym).copied()
    }

    // P19.14
    /// `true` iff `sub` is `sup` or any of its
    /// transitive supertypes is `sup`. Bounded at 32 hops.
    pub fn is_subtype_of(&self, sub: &str, sup: &str) -> bool {
        if sub == sup {
            return true;
        }
        let Some(target) = self.symbols.lookup(sup) else {
            return false;
        };
        let Some(mut cur) = self.symbols.lookup(sub) else {
            return false;
        };
        for _ in 0..MAX_SUPERTYPE_CHAIN_DEPTH {
            let Some(members) = self.type_members.get(&cur) else {
                return false;
            };
            let Some(parent) = members.supertype else {
                return false;
            };
            if parent == target {
                return true;
            }
            cur = parent;
        }
        false
    }

    // P36.1
    /// Handle-keyed variant of [`Self::is_subtype_of`]. Resolves both
    /// handles to their declared names via `registry` and delegates
    /// to the name-keyed implementation. Returns `false` when either
    /// handle isn't in the registry (callers that mint handles via
    /// the project pipeline always insert both via the same registry,
    /// so a registered-but-missing handle is a programming error,
    /// not a user-facing case).
    ///
    /// Replaces every `is_subtype_of(&str, &str)` call site as the
    /// migration progresses; the string form is kept during the
    /// `Named` -> `Type(handle)` cascade and deleted in P36.7.
    pub fn is_subtype_of_decl(&self, arena: &TypeArena, sub: TypeDeclId, sup: TypeDeclId) -> bool {
        if sub == sup {
            return true;
        }
        let (Some(sub_name), Some(sup_name)) = (arena.decl_name(sub), arena.decl_name(sup)) else {
            return false;
        };
        self.is_subtype_of(sub_name, sup_name)
    }
    pub fn native_for(&self, name: &str) -> Option<&NativeSignature> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.natives.lookup_sym(s))
    }
    pub fn type_flags_for(&self, name: &str) -> Option<&TypeFlags> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.type_flags.get(&s))
    }
    pub fn contains_value(&self, name: &str) -> bool {
        self.symbols
            .lookup(name)
            .is_some_and(|s| self.values.contains(&s))
    }
    pub fn contains_type_member(&self, name: &str) -> bool {
        self.symbols
            .lookup(name)
            .is_some_and(|s| self.type_members.contains_key(&s))
    }
    pub fn contains_fn_signature(&self, name: &str) -> bool {
        self.symbols
            .lookup(name)
            .is_some_and(|s| self.fn_signatures.contains_key(&s))
    }
    // P38.1
    /// `true` iff `name` was ingested as a non-native top-level `fn`
    /// in any module. Distinct from [`Self::contains_fn_signature`]
    /// (natives only). Lets the analyzer's `Definition::ProjectDecl`
    /// value-typing arm produce `function` for cross-module bare fn
    /// idents without first walking the foreign HIR.
    pub fn contains_non_native_fn(&self, name: &str) -> bool {
        self.symbols
            .lookup(name)
            .is_some_and(|s| self.non_native_fn_names.contains(&s))
    }
    // P38.4
    /// `true` iff the decl at `(uri, decl_id)` was ingested with the
    /// `private` modifier. Lets the resolver filter cross-module
    /// candidates by visibility for bare-name lookup while leaving
    /// the FQN path unaffected.
    pub fn is_decl_private(&self, uri: &Uri, decl_id: Idx<Decl>) -> bool {
        self.private_locations.contains(&(uri.clone(), decl_id))
    }

    /// Walk a HIR module's top-level decls and register everything
    /// that's a type-name (type / enum) or a native function. Mints
    /// every encountered decl into `decl_registry` (the project-wide
    /// `(uri, decl_id) → TypeDeclId` interner) and well-known
    /// `(lib, module, name)` slots, allocates the enum's
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
        decl_registry: &mut crate::well_known::DeclRegistry,
        well_known: &mut crate::well_known::WellKnown,
    ) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        // P15.x — capture the module's name (URI's filename stem
        // without `.gcl`) so resolver / pass 3.5 can recognize
        // `module::Decl` chains.
        if let Some(name) = module_name_from_uri(uri) {
            let sym = self.symbols.intern(&name);
            self.module_names.insert(sym, uri.clone());
        }
        for decl_id in &module.decls {
            let modifiers = match &hir.decls[*decl_id] {
                Decl::Type(td) => {
                    let name_str = hir.idents[td.name].text.as_str();
                    let name_sym = self.symbols.intern(name_str);
                    // Recognised type name (drives `has_name` and the
                    // sig-cache fingerprint).
                    self.type_names.insert(name_sym);
                    // Project-wide handle for this decl + well-known
                    // slot recording. Folded in from the former
                    // standalone pre-pass in
                    // `stage_lower_signatures` so the project has a
                    // single decl-registration point.
                    let handle = decl_registry.get_or_insert(uri, *decl_id);
                    well_known.record(&module.lib, &module.name, name_str, handle);
                    // P13.5: capture @iterable / @deref / @primitive
                    // flag bits into the per-type table.
                    let flags = derive_type_flags(&td.modifiers.annotations);
                    if flags.iterable || flags.deref.is_some() || flags.primitive {
                        self.type_flags.entry(name_sym).or_insert(flags);
                    }
                    // P21 — populate the cross-module member index.
                    // First-decl-wins (matches `decl_locations`'s
                    // collision semantics). Lets the per-module
                    // analyzer's `resolve_member` bind foreign attrs /
                    // methods inline instead of deferring to a post
                    // pass.
                    if !self.type_members.contains_key(&name_sym) {
                        let generics: Vec<Symbol> = td
                            .generics
                            .iter()
                            .map(|g| self.symbols.intern(hir.idents[*g].text.as_str()))
                            .collect();
                        // **P19.14** — capture the direct supertype
                        // name (the `Super` in `type Sub extends
                        // Super`). Resolved as a Symbol now (without
                        // looking up the supertype's TypeMembers,
                        // which may not be ingested yet — order is
                        // module-dependent). Lookup walks the chain
                        // lazily on access.
                        let supertype = td.supertype.and_then(|tr| {
                            let parent_text = hir.idents[hir.type_refs[tr].name].text.as_str();
                            // Skip the trivial primitives that can
                            // never be a user type's supertype —
                            // they'd never resolve to a TypeMembers
                            // entry anyway and the noise pollutes
                            // the symbol table only marginally.
                            if matches!(
                                parent_text,
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
                                None
                            } else {
                                Some(self.symbols.intern(parent_text))
                            }
                        });
                        let mut m = TypeMembers {
                            home_uri: uri.clone(),
                            attrs: FxHashMap::default(),
                            methods: FxHashMap::default(),
                            generics,
                            // P22 — `attr_types` / `method_returns`
                            // get filled in by
                            // `ProjectAnalysis::stage_lower_signatures`
                            // after every module is loaded.
                            attr_types: FxHashMap::default(),
                            method_returns: FxHashMap::default(),
                            static_attrs: FxHashSet::default(),
                            static_methods: FxHashSet::default(),
                            abstract_methods: FxHashSet::default(),
                            supertype,
                            // Filled in by `apply_module_contributions`
                            // after signature lowering — see
                            // `populate_deref_caches` in
                            // [`crate::project`].
                            deref_return_ty: None,
                        };
                        for attr_id in &td.attrs {
                            let attr = &hir.type_attrs[*attr_id];
                            let attr_sym = self.symbols.intern(hir.idents[attr.name].text.as_str());
                            m.attrs.insert(attr_sym, *attr_id);
                            // P19.13 — capture `static` flag at
                            // ingest time so `Expr::Static` value
                            // typing can distinguish static-attr
                            // value access from a runtime `field`
                            // handle, even for cross-module attrs.
                            if attr.modifiers.static_ {
                                m.static_attrs.insert(attr_sym);
                            }
                        }
                        for method_id in &td.methods {
                            if let Decl::Fn(fnd) = &hir.decls[*method_id] {
                                let method_sym =
                                    self.symbols.intern(hir.idents[fnd.name].text.as_str());
                                m.methods.insert(method_sym, *method_id);
                                if fnd.modifiers.static_ {
                                    m.static_methods.insert(method_sym);
                                }
                                if fnd.modifiers.abstract_ {
                                    m.abstract_methods.insert(method_sym);
                                }
                            }
                        }
                        self.type_members.insert(name_sym, m);
                    }
                    self.record_decl_location(name_sym, uri, *decl_id);
                    Some(&td.modifiers)
                }
                Decl::Enum(ed) => {
                    let name_str = hir.idents[ed.name].text.as_str();
                    let name_sym = self.symbols.intern(name_str);
                    self.type_names.insert(name_sym);
                    // Project-wide decl handle (enums get one too — the
                    // resolver / lowering paths route foreign enum refs
                    // through `Type(handle)` for some shapes).
                    let _ = decl_registry.get_or_insert(uri, *decl_id);
                    // Alloc the canonical `TypeKind::Enum` into the
                    // shared arena and publish to `enum_types`
                    // immediately. Doing it here (rather than as a
                    // side effect of `lower_module_signatures`'
                    // `apply_module_contributions`) ensures every
                    // downstream lowering pass — including method
                    // return-type lowering for methods declared in
                    // the same module — sees the canonical TypeId.
                    self.enum_types.entry(name_sym).or_insert_with(|| {
                        let name_owned: SmolStr = name_str.into();
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
                    self.record_decl_location(name_sym, uri, *decl_id);
                    Some(&ed.modifiers)
                }
                Decl::Fn(fnd) => {
                    let name_str = hir.idents[fnd.name].text.as_str();
                    let name_sym = self.symbols.intern(name_str);
                    if fnd.modifiers.native {
                        let sig = native_signature_for(
                            hir,
                            fnd,
                            arena,
                            decl_registry,
                            &self.decl_locations,
                            &self.symbols,
                        );
                        self.natives.register(name_sym, sig);
                    } else {
                        self.values.insert(name_sym);
                        // P38.1 — tag non-native fn names so the
                        // analyzer's `Definition::ProjectDecl`
                        // value-typing arm can route them to
                        // `function_ty()` instead of falling through
                        // to `type_ty()` via `has_name`.
                        self.non_native_fn_names.insert(name_sym);
                    }
                    self.record_decl_location(name_sym, uri, *decl_id);
                    Some(&fnd.modifiers)
                }
                Decl::Var(vd) => {
                    let name_sym = self.symbols.intern(hir.idents[vd.name].text.as_str());
                    self.values.insert(name_sym);
                    self.record_decl_location(name_sym, uri, *decl_id);
                    Some(&vd.modifiers)
                }
                Decl::Pragma(p) => {
                    // P13.6: capture `@permission("name")` mod-pragmas
                    // into the project-wide `module_permissions` map.
                    if hir.idents[p.name].text == "permission"
                        && let Some(arg_expr) = p.args.first()
                        && let greycat_analyzer_hir::types::Expr::String(s) = &hir.exprs[*arg_expr]
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
            // P13.4: walk modifiers' annotations for `@expose("name")`
            // and capture the rename target into the project-wide
            // exposed map.
            if let Some(modifiers) = modifiers {
                // P38.4 — tag private decls so the resolver's
                // bare-name lookup can filter them out of the
                // cross-module candidate set. The decl stays in
                // `decl_locations` (the FQN path still needs to
                // reach it — see probe p5).
                if modifiers.private {
                    self.private_locations.insert((uri.clone(), *decl_id));
                }
                let local_name = hir.decls[*decl_id]
                    .name()
                    .map(|n| hir.idents[n].text.clone())
                    .unwrap_or_default();
                for ann in &modifiers.annotations {
                    if ann.name != "expose" {
                        continue;
                    }
                    let rename = ann.args.first().cloned();
                    let key_str = rename.as_deref().unwrap_or(local_name.as_str());
                    let key_sym = self.symbols.intern(key_str);
                    let entries = self.exposed.entry(key_sym).or_default();
                    let already = entries
                        .iter()
                        .any(|s| s.uri == *uri && s.decl == *decl_id && s.rename == rename);
                    if !already {
                        entries.push(ExposureSite {
                            uri: uri.clone(),
                            decl: *decl_id,
                            local_name: local_name.clone(),
                            rename,
                        });
                    }
                }
            }
        }
        self.modules_ingested += 1;
    }

    // P19.9
    /// `Symbol`-keyed location index. Caller must have
    /// already interned `name_sym` through `self.symbols`.
    fn record_decl_location(&mut self, name_sym: Symbol, uri: &Uri, decl_id: Idx<Decl>) {
        let entry = self.decl_locations.entry(name_sym).or_default();
        if !entry.iter().any(|(u, d)| u == uri && *d == decl_id) {
            entry.push((uri.clone(), decl_id));
        }
    }

    /// Cross-module decl lookup: every `(Uri, Idx<Decl>)` pair
    /// known under this name. Empty slice when the name is unknown.
    /// Built-in runtime type names (`Array`, `Map`, …) and language
    /// primitives have no `.gcl` decl and so never appear here — use
    /// [`Self::has_name`] to ask the broader "is this name known?"
    /// question.
    pub fn locate_decl(&self, name: &str) -> &[(Uri, Idx<Decl>)] {
        match self.symbols.lookup(name) {
            Some(s) => self.locate_decl_by_symbol(s),
            None => &[],
        }
    }

    pub fn locate_decl_by_symbol(&self, name: Symbol) -> &[(Uri, Idx<Decl>)] {
        self.decl_locations
            .get(&s)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// `true` iff `name` resolves against any name the project knows:
    /// a registered type / enum, a native fn signature, or a top-level
    /// non-native fn / var. Resolver uses this as the post-local-scope
    /// fallback.
    pub fn has_name(&self, name: &str) -> bool {
        let Some(sym) = self.symbols.lookup(name) else {
            return false;
        };
        self.type_names.contains(&sym)
            || self.natives.signatures.contains_key(&sym)
            || self.values.contains(&sym)
    }

    // P15.x
    /// `true` iff `name` matches a known module (any ingested
    /// doc whose filename stem equals `name`). Lets the resolver
    /// recognize `runtime` in `runtime::Identity::create` as a module.
    pub fn has_module(&self, name: &str) -> bool {
        self.symbols
            .lookup(name)
            .is_some_and(|s| self.module_names.contains_key(&s))
    }

    // P15.x
    /// Return the URI of the module whose filename stem
    /// matches `name`, if any.
    pub fn module_uri(&self, name: &str) -> Option<&Uri> {
        self.symbols
            .lookup(name)
            .and_then(|s| self.module_names.get(&s))
    }
}

// P15.x
/// Extract the module name from a URI (filename without
/// `.gcl`). Mirrors [`Document::name`](greycat_analyzer_core::Document)
/// without the borrow on a manager.
pub fn module_name_from_uri(uri: &Uri) -> Option<String> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    let last = stripped.rsplit(['/', '\\']).next()?;
    let stem = last.strip_suffix(".gcl").unwrap_or(last);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
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

// P19.16
/// Runtime-exposed value-position globals. The runtime
/// makes these names available at value position with a fixed type;
/// they have no `.gcl` declaration, so the resolver/analyzer must seed
/// them. `(name, primitive)` pairs — extend as new runtime globals are
/// confirmed against `greycat run`.
pub const BUILTIN_RUNTIME_GLOBALS: &[(&str, Primitive)] =
    &[("Infinity", Primitive::Float), ("NaN", Primitive::Float)];

// P13.5
/// Read `@iterable` / `@deref` / `@primitive` annotations on a
/// type decl into a [`TypeFlags`] record.
fn derive_type_flags(annotations: &[Annotation]) -> TypeFlags {
    let mut flags = TypeFlags::default();
    for ann in annotations {
        match ann.name.as_str() {
            "iterable" => flags.iterable = true,
            "primitive" => flags.primitive = true,
            "deref" => flags.deref = ann.args.first().cloned().or(Some(SmolStr::default())),
            _ => {}
        }
    }
    flags
}

fn native_signature_for(
    hir: &Hir,
    fnd: &FnDecl,
    arena: &mut TypeArena,
    decl_registry: &crate::well_known::DeclRegistry,
    locate_decl: &FxHashMap<Symbol, Vec<(Uri, Idx<Decl>)>>,
    symbols: &SymbolTable,
) -> NativeSignature {
    let params = fnd
        .params
        .iter()
        .map(|p_id| {
            let p = &hir.fn_params[*p_id];
            p.ty.map(|t| lower_native_type_ref(hir, t, arena, decl_registry, locate_decl, symbols))
                .unwrap_or_else(|| arena.any())
        })
        .collect();
    let return_ty = fnd
        .return_type
        .map(|t| lower_native_type_ref(hir, t, arena, decl_registry, locate_decl, symbols))
        .unwrap_or_else(|| arena.any());
    NativeSignature { params, return_ty }
}

/// Native-fn signature counterpart of
/// [`crate::project::lower_type_ref_project`]. Same handle-keyed
/// resolution shape — every reference to a `.gcl`-declared type
/// mints `Type(handle)` / `Generic(handle, args)` via the
/// project's `decl_registry`, never the legacy `Named` fallback.
/// Falls back to `Unresolved` when the referenced decl hasn't been
/// ingested yet (rare — native fns typically reference primitives
/// or stdlib types declared earlier in the same module).
fn lower_native_type_ref(
    hir: &Hir,
    idx: Idx<HirTypeRef>,
    arena: &mut TypeArena,
    decl_registry: &DeclRegistry,
    locate_decl: &FxHashMap<Symbol, Vec<(Uri, Idx<Decl>)>>,
    symbols: &SymbolTable,
) -> TypeId {
    let tr = &hir.type_refs[idx];
    let name_sym = hir.idents[tr.name].symbol;
    let mut base = match &symbols[name_sym] {
        "bool" => arena.primitive(Primitive::Bool),
        "int" => arena.primitive(Primitive::Int),
        "float" => arena.primitive(Primitive::Float),
        "char" => arena.primitive(Primitive::Char),
        "String" => arena.primitive(Primitive::String),
        "time" => arena.primitive(Primitive::Time),
        "duration" => arena.primitive(Primitive::Duration),
        "geo" => arena.primitive(Primitive::Geo),
        "any" => arena.any(),
        "null" => arena.null(),
        _ => {
            // Resolve the decl handle once (the same lookup powers both
            // the generic and non-generic branches).
            let handle = locate_decl.get(&name_sym).and_then(|locs| {
                locs.iter()
                    .find_map(|(uri, decl)| decl_registry.lookup(uri, *decl))
            });
            if !tr.params.is_empty() {
                let args: Vec<TypeId> = tr
                    .params
                    .iter()
                    .map(|p| {
                        lower_native_type_ref(hir, *p, arena, decl_registry, locate_decl, symbols)
                    })
                    .collect();
                match handle {
                    Some(h) => arena.generic(h, args),
                    None => arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end)),
                }
            } else {
                // Try to mint a handle-keyed `Type(handle)` so native
                // signatures intern equal to whatever the body-walker
                // / signature pass produces for the same source token.
                match handle {
                    Some(h) => arena.alloc_type(h),
                    None => arena.unresolved(name, (tr.byte_range.start, tr.byte_range.end)),
                }
            }
        }
    };
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;
    use std::str::FromStr;

    fn lower(src: &str) -> Hir {
        let tree = parse(src);
        let s = SymbolTable::default();
        lower_module(src, &s, "stdmod", "std", tree.root_node())
    }

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).unwrap()
    }

    /// Spin up the four pieces of state a real `ProjectAnalysis`
    /// threads through `ingest` — the shared `TypeArena`, the
    /// decl-handle interner, the well-known-slot table, and the
    /// index itself. Returned by-value so each test owns an
    /// independent copy.
    fn fresh_index() -> (
        TypeArena,
        crate::well_known::DeclRegistry,
        crate::well_known::WellKnown,
        ProjectIndex,
    ) {
        let mut arena = TypeArena::new();
        let decl_registry = crate::well_known::DeclRegistry::default();
        let well_known = crate::well_known::WellKnown::default();
        let idx = ProjectIndex::new(&mut arena);
        (arena, decl_registry, well_known, idx)
    }

    #[test]
    fn ingest_registers_type_decls() {
        let hir = lower(
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
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(
            &uri("/proj/people.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        assert_eq!(idx.modules_ingested, 1);
        assert!(idx.has_name("Person"));
        assert!(idx.has_name("Company"));
    }

    #[test]
    fn ingest_registers_enum_decls() {
        let hir = lower("enum Color { Red, Green, Blue }\n");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(
            &uri("/proj/color.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        let id = idx.enum_type_for("Color").expect("Color registered");
        let ty = arena.get(id);
        let TypeKind::Enum { variants, .. } = &ty.kind else {
            panic!("expected enum, got {ty:?}");
        };
        assert_eq!(variants, &["Red", "Green", "Blue"]);
    }

    #[test]
    fn ingest_captures_native_signatures() {
        let hir = lower(
            r#"
private native fn read_file(path: String): String;
private native fn now(): time;
"#,
        );
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(
            &uri("/proj/io.gcl"),
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );
        let read = idx.native_for("read_file").expect("read_file present");
        assert_eq!(read.params.len(), 1);
        let now = idx.native_for("now").expect("now present");
        assert!(now.params.is_empty());
    }

    #[test]
    fn ingest_is_idempotent_on_repeated_calls() {
        let hir = lower("type T {}\n");
        let u = uri("/proj/t.gcl");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        let len_after_first = arena.len();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        assert_eq!(arena.len(), len_after_first, "duplicate type registrations");
        assert_eq!(idx.modules_ingested, 2);
        // decl_locations is also idempotent — the same (uri, decl_id)
        // pair shouldn't be appended twice.
        assert_eq!(idx.locate_decl("T").len(), 1);
    }

    #[test]
    fn locate_decl_records_uri_and_decl_id() {
        // Acceptance for P11.1: querying the index for a declared type
        // returns the URI of the module that introduced it and a
        // matching `Idx<Decl>`. Synthetic stand-in for `Permission` in
        // `lib/std/runtime.gcl` so the test doesn't depend on `greycat
        // install` having been run.
        let hir = lower("private type Permission {}\n");
        let permission_uri = uri("/proj/lib/std/runtime.gcl");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(
            &permission_uri,
            &hir,
            &mut arena,
            &mut decl_registry,
            &mut well_known,
        );

        let hits = idx.locate_decl("Permission");
        assert_eq!(hits.len(), 1, "exactly one Permission decl across project");
        let (found_uri, decl_id) = &hits[0];
        assert_eq!(found_uri, &permission_uri);
        assert!(matches!(&hir.decls[*decl_id], Decl::Type(_)));
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
        let hir = lower(
            r#"
type Bag {
    weight: int;
    fn lift(): int;
}

enum Color { Red, Green }

fn helper(): int { return 1; }
"#,
        );
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
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
            assert_eq!(idx.symbols.resolve(&sym), Some(n));
        }

        // The new `&str` accessors hit through the symbol table.
        let bag = idx.type_members_for("Bag").expect("Bag in type_members");
        assert!(bag.attr_id(&idx.symbols, "weight").is_some());
        assert!(bag.method_id(&idx.symbols, "lift").is_some());

        // Same Symbol is shared between the outer index and the
        // inner TypeMembers — interning preserves identity.
        let weight_sym = idx.symbol("weight").expect("weight interned via ingest");
        assert!(bag.attrs.contains_key(&weight_sym));
    }

    // P36.1
    #[test]
    fn is_subtype_of_decl_resolves_handles_then_delegates_to_name_keyed() {
        // Inheritance graph: `Cat extends Animal`. `is_subtype_of_decl`
        // takes handles, looks each up in the arena's decl-names table,
        // and asks the existing name-keyed walker. Equal handles
        // short-circuit without arena access; missing handles return
        // false.
        use crate::well_known::DeclRegistry;
        use greycat_analyzer_core::TypeArena;
        use greycat_analyzer_hir::arena::Idx;
        use greycat_analyzer_hir::types::Decl;

        let hir = lower(
            "type Animal { name: String; }\n\
             type Cat extends Animal { whiskers: int; }\n",
        );
        let u = uri("/proj/m.gcl");
        let (mut idx_arena, mut idx_decl_registry, mut idx_well_known, mut idx) = fresh_index();
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
                let id = registry.get_or_insert(&u, *decl_id);
                arena.alloc_type(id);
                match &idx.symbols[hir.idents[td.name].symbol] {
                    "Animal" => animal = Some(id),
                    "Cat" => cat = Some(id),
                    _ => {}
                }
            }
        }
        let animal = animal.unwrap();
        let cat = cat.unwrap();

        assert!(idx.is_subtype_of_decl(&arena, cat, animal));
        assert!(!idx.is_subtype_of_decl(&arena, animal, cat));
        // Reflexivity short-circuits regardless of arena membership.
        assert!(idx.is_subtype_of_decl(&arena, animal, animal));

        // A handle whose name was never registered in the arena returns
        // false (no panic).
        let dangling = registry.get_or_insert(&uri("/other.gcl"), Idx::<Decl>::from_raw(99u32));
        assert!(!idx.is_subtype_of_decl(&arena, dangling, animal));
    }

    #[test]
    fn locate_decl_keeps_collisions_across_modules() {
        // Same name in two modules should produce two entries — P11.2
        // disambiguates at the use site via the importer's lib/include
        // closure, but the table itself keeps every hit.
        let hir_a = lower("type Helper {}\n");
        let hir_b = lower("type Helper {}\n");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
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
        let hits = idx.locate_decl("Helper");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, uri("/proj/a.gcl"));
        assert_eq!(hits[1].0, uri("/proj/b.gcl"));
    }

    #[test]
    fn ingest_captures_expose_rename_into_exposed_map() {
        // P13.4: `@expose("renamed")` keys into ProjectIndex::exposed by
        // the renamed string; bare `@expose` keys by the decl's local
        // name.
        let hir = lower(
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
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let alpha_sym = idx.symbol("public_alpha").expect("public_alpha interned");
        let alpha_hits = idx.exposed.get(&alpha_sym).expect("public_alpha");
        assert_eq!(alpha_hits.len(), 1);
        assert_eq!(alpha_hits[0].rename.as_deref(), Some("public_alpha"));
        assert_eq!(alpha_hits[0].local_name, "alpha");

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
        let hir = lower(
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
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);

        let bag = idx.type_flags_for("Bag").expect("Bag flags");
        assert!(bag.iterable);
        assert_eq!(bag.deref.as_deref(), Some("resolve"));
        assert!(!bag.primitive);

        let marker = idx.type_flags_for("Marker").expect("Marker flags");
        assert!(marker.primitive);
        assert!(!marker.iterable);

        // Plain has no annotations — kept out of the map.
        assert!(idx.type_flags_for("Plain").is_none());
    }

    #[test]
    fn ingest_captures_permission_pragmas_per_module() {
        // P13.6: `@permission("name")` pragma populates
        // ProjectIndex::module_permissions[uri].
        let hir = lower("@permission(\"admin\");\n@permission(\"user\");\nfn handler() {}\n");
        let u = uri("/proj/api.gcl");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
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
        let hir = lower(
            r#"
fn helper(): int { return 1; }
var TOP: int = 1;
"#,
        );
        let u = uri("/proj/m.gcl");
        let (mut arena, mut decl_registry, mut well_known, mut idx) = fresh_index();
        idx.ingest(&u, &hir, &mut arena, &mut decl_registry, &mut well_known);
        assert_eq!(idx.locate_decl("helper").len(), 1);
        assert_eq!(idx.locate_decl("TOP").len(), 1);
        assert!(idx.locate_decl("missing").is_empty());
    }
}
