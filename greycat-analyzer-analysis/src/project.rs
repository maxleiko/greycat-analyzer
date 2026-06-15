//! Project-level analyzer driver.
//!
//! Builds a [`ProjectAnalysis`] over a [`SourceManager`] in one pass:
//! lower every document to HIR, ingest each module's type / enum /
//! native decls into a shared [`ProjectIndex`], then run the per-module
//! resolver + analyzer + lints. The result is cached so subsequent LSP
//! `publish_for` calls and CLI lint runs that span many files don't
//! rebuild the whole pipeline per file.
//!
//! The chunk's "shared `ProjectIndex`" is populated here from every
//! module's top-level decls; rerouting the per-module analyzer to
//! consult it for cross-module name lookup is **** territory.
//! gives that work the cache-shaped seam to plug into.

use std::hash::{DefaultHasher, Hash, Hasher};

// `web-time` is a transparent drop-in for `std::time` — re-exports
// the std types on native, falls back to `performance.now()` on
// `wasm32-unknown-unknown` (where `std::time::Instant::now()` panics
// with "time not implemented on this platform"). The crate's whole
// purpose is to be cfg-gated internally, so consumers don't need to.
use web_time::{Duration, Instant};

use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_syntax::tree_sitter::{Node, Tree};
use rustc_hash::{FxHashMap, FxHashSet};

use greycat_analyzer_core::TypeRegistry;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::{
    Builtins, GenericOwner, ItemKey, SourceManager, Symbol, SymbolTable, TypeArena, TypeId,
    TypeKind,
};
use greycat_analyzer_hir::hir::{BlockStmt, Decl, Expr, Ident, Stmt, TypeRef};
use greycat_analyzer_hir::{DeclRegistry, Hir, lower_module};

use crate::analyzer::{AnalysisResult, DiagCategory, SemanticDiagnostic, analyze_with_index_into};
use crate::directives::{Directives, parse_directives};
use crate::display::{ProjectTypeDisplay, display_type, display_type_for_module};
use crate::index::{FnSignature, ProjectIndex, module_name_from_uri};
use crate::lint::{
    self, LintDiagnostic, SURFACED_RULES, lint_arrow_on_non_deref_with_directives,
    lint_catch_empty_parens, lint_inferred_return_type_with_directives, lint_no_breakpoint,
    lint_non_exhaustive_with_directives, lint_nullability_with_directives,
    lint_redundant_semicolon, lint_surfaced_with_directives, lint_unreachable_with_directives,
    lint_unused_suppressions, run_lints_with_directives,
};
use crate::lower_type_ref::{self, TypeRefLowering};
use crate::meta_pragmas::{LintPragmas, parse_lint_pragmas};
use crate::resolver::Resolutions;

/// Per-document outputs of the analyzer pipeline. Held by
/// [`ProjectAnalysis`] so LSP / CLI consumers can pull diagnostics
/// without re-running lower → resolve → analyze for the same text.
#[derive(Debug)]
pub struct ModuleAnalysis {
    pub hir: Hir,
    pub resolutions: Resolutions,
    pub analysis: AnalysisResult,
    pub lints: Vec<LintDiagnostic>,
    /// Library this module belongs to — copied from
    /// [`greycat_analyzer_core::Document::lib`] at construction.
    /// `"project"` for the user's own modules, `"std"` /
    /// `"explorer"` / etc. for vendored deps under `lib/`. CLI / LSP
    /// consumers filter on this to skip lib-owned lints by default
    /// (see `cli lint --lint-libs`).
    pub lib: String,
    // P14.5
    /// Per-phase wall-clock timings captured during the
    /// last `rebuild` / `invalidate`. Useful for surfacing where the
    /// pipeline spends its time (`cli lint --csv`).
    pub timings: ModuleTimings,
    // P23.1
    /// Directive set parsed from the source's `// gcl-…`
    /// comments. Drives lint suppressions ([`run_lints_with_directives`])
    /// and is consulted by the formatter when this module is being
    /// re-rendered.
    pub directives: Directives,
}

/// Per-module pipeline timings.
#[derive(Debug, Default, Clone, Copy)]
pub struct ModuleTimings {
    /// Time spent in `lower_module` (CST → HIR walker).
    pub lower: Duration,
    /// Resolver pass (`resolve_with_index`).
    pub resolve: Duration,
    /// Analyzer pass (`analyze_with_index`).
    pub analyze: Duration,
    /// Lint rules (`run_lints`).
    pub lint: Duration,
}

impl ModuleTimings {
    /// Sum of every recorded phase. Doesn't include `parse` / file
    /// I/O, which lives in `LoadReport.loaded`'s per-uri duration.
    pub fn total(&self) -> Duration {
        self.lower + self.resolve + self.analyze + self.lint
    }
}

/// Project-level analysis cache.
///
/// `index` is rebuilt from every module's HIR each time the cache is
/// (re)populated, so removed type / enum / native decls are reflected
/// instead of lingering.
///
/// The [`TypeArena`] now lives on the
/// project (not per [`AnalysisResult`]). Every module's analyzer mints
/// into the same arena so cross-module `TypeId`s are directly
/// comparable — no `mint_type_shape`/`read_type_shape` translation
/// needed. Callers that previously wrote `module.analysis.types`
/// should call [`Self::arena`] / [`Self::arena_mut`] instead.
#[derive(Debug)]
pub struct ProjectAnalysis {
    pub index: ProjectIndex,
    /// Project-wide type arena. Populated alongside every
    /// module's analyzer pass. Append-only and interned, so duplicate
    /// `seed_builtins` calls per `analyze_with_index_into` are a
    /// no-op.
    pub arena: TypeArena,
    /// Project-wide registry of resolved `(Uri, Idx<Decl>)` →
    /// [`TypeDeclId`]. Issued during signature lowering.
    pub decl_registry: DeclRegistry,
    /// When `true`, lint suppressions (`// gcl-lint-off …`)
    /// are still recorded but never silence emissions. Drives the CLI's
    /// `--no-suppressions` flag.
    pub bypass_suppressions: bool,
    /// Names of rules the caller has explicitly enabled. Only matters
    /// for rules that ship default-off (`default_enabled = false` in
    /// [`LINT_RULES`]); default-on rules are always active. Drives the
    /// CLI's `--on=<rule>` flag, the entrypoint's `@lint_on("…")`
    /// project pragmas (P40), and any future LSP config equivalent.
    pub enabled_rules: FxHashSet<String>, // FIXME: should be FxHashSet<&'static str>
    /// Names of rules disabled project-wide. Populated by the
    /// entrypoint's `@lint_off("…")` pragmas. `enabled_rules` and
    /// `disabled_rules` together describe project-wide policy; when
    /// both name the same rule, `disabled_rules` wins (explicit
    /// silence beats explicit enable — matches the CLI precedent of
    /// `--off=X --on=X` silencing X).
    pub disabled_rules: FxHashSet<String>, // FIXME: should be FxHashSet<&'static str>
    modules: FxHashMap<Uri, ModuleAnalysis>,
    /// Per-module signature-stage cache. Records what each
    /// module contributed to the project signature index
    /// (`attr_types`, `method_returns`, `fn_signatures`, `enum_types`)
    /// during the last [`lower_signatures_into`] call, plus the hash
    /// of the bytes that produced it. The arena is append-only across
    /// `invalidate` (only `reset_state` clears it), so cached
    /// `TypeId`s remain valid as long as the cache is dropped on
    /// reset. The stored `name_set_hash` reflects the project-wide
    /// name set used during lowering — when a module's
    /// `lower_type_ref_project` outcome depends on which names exist
    /// in `index`, both hashes must match the new state to reuse the
    /// cached contributions.
    sig_cache: FxHashMap<Uri, ModuleSigCache>,
}

/// Per-module lowering data structures
struct LoweredModule {
    uri: Uri,
    hir: Hir,
    lib: String,
    lower_took: Duration,
    directives: Directives,
    pragmas: LintPragmas,
}

/// What one module contributed to the project signature index.
#[derive(Debug, Clone, Default)]
struct ModuleSigCache {
    /// Hash of every byte the lowering pass reads out of this module's
    /// HIR (decl names, generics, attr / method / fn / enum
    /// signatures + every TypeRef structure they reach). Body
    /// statements / expressions are intentionally excluded.
    sig_hash: u64,
    /// Hash of the project-wide name set captured during the lowering
    /// (the set of names that `index.has_name` would return `true`
    /// for). Cached contributions are only reusable when this matches
    /// the post-ingest project state — otherwise a previously-`any()`
    /// reference to a now-known type would silently stay `any()`.
    name_set_hash: u64,
    /// `(type_id, attr_sym, ty)` — attr name stays bare Symbol
    /// (per-type-internal).
    attrs: Vec<(ItemKey, Symbol, TypeId)>,
    /// `(type_id, method_sym, ty)`.
    methods: Vec<(ItemKey, Symbol, TypeId)>,
    /// `(type_id, method_sym, FnSignature)` — full signature for
    /// methods that declare their own generic params (`<T, …>`).
    /// Lets cross-module / static / instance / arrow method calls
    /// run the same witness-based generic inference the bare-Ident
    /// path uses today (see [`crate::analyzer::Cx::try_generic_call_inference`]).
    method_sigs: Vec<(ItemKey, Symbol, FnSignature)>,
    /// `(fn_id, signature)`.
    fns: Vec<(ItemKey, FnSignature)>,
    /// `(var_id, ty)`. Top-level `var` declared types.
    /// Lowered alongside the other signatures in
    /// [`lower_module_signatures`] so the analyzer's bare-Ident path
    /// can type a cross-module `Definition::ProjectDecl` pointing at
    /// a var.
    vars: Vec<(ItemKey, TypeId)>,
    /// `(type_id, supertype_ty)`. Pre-lowered direct supertype shape
    /// (e.g. `Generic { decl: Base, args: [int] }` for
    /// `Sub extends Base<int>`). Populated alongside the other
    /// signatures so `apply_module_contributions` can write back the
    /// instantiated parent TypeId into `TypeMembers::supertype_ty`
    /// — used by `is_assignable_to_with_index` to walk the chain with
    /// real generic args, not just decl identity.
    supertypes: Vec<(ItemKey, TypeId)>,
}

impl Default for ProjectAnalysis {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectAnalysis {
    pub fn new() -> Self {
        let symbols = SymbolTable::new();
        let arena = TypeArena::new(&symbols);
        let index = ProjectIndex::new(symbols, &arena);
        Self {
            index,
            arena,
            decl_registry: DeclRegistry::new(),
            bypass_suppressions: false,
            enabled_rules: FxHashSet::default(),
            disabled_rules: FxHashSet::default(),
            modules: FxHashMap::default(),
            sig_cache: FxHashMap::default(),
        }
    }

    /// Borrow the project-wide type arena — required for any
    /// `TypeId` lookup (`arena.get(id)`, `display(arena, id)`, …).
    #[inline(always)]
    pub fn arena(&self) -> &TypeArena {
        &self.arena
    }

    /// Mutable borrow of the project-wide type arena. Capability
    /// handlers should not mint new types; this is reserved for the
    /// orchestrator and the staged-pipeline body walker.
    #[inline(always)]
    pub fn arena_mut(&mut self) -> &mut TypeArena {
        &mut self.arena
    }

    /// Borrow the project-wide symbol table
    #[inline(always)]
    pub fn symbols(&self) -> &SymbolTable {
        &self.index.symbols
    }

    /// Project-wide decl-handle registry — the canonical
    /// `(Uri, Idx<Decl>) → TypeDeclId` interner. Decl *names* live
    /// here too: capability handlers thread the registry into
    /// [`display_type`] / [`display_fqn`] so decl-keyed types render
    /// as their source name.
    #[inline(always)]
    pub fn decl_registry(&self) -> &DeclRegistry {
        &self.decl_registry
    }

    /// Project-aware type display. Renders the type with a
    /// `<module>::` qualifier prefix whenever the bare decl name is
    /// ambiguous within the project (≥2 modules export it). When the
    /// name is unique, output matches [`display_type`] byte-for-byte.
    ///
    /// Used by inlay hints so the user reading `var f = b::Foo {};`
    /// sees `: b::Foo` rather than the ambiguous bare `: Foo`.
    pub fn display_type(&self, ty: TypeId) -> ProjectTypeDisplay<'_> {
        ProjectTypeDisplay {
            project: self,
            id: ty,
        }
    }

    /// Resolve a `Symbol` back to its source text through the
    /// project's [`SymbolTable`].
    #[inline(always)]
    pub fn symbol(&self, sym: &Symbol) -> &str {
        &self.index.symbols[*sym]
    }

    /// Resolve an [`ItemKey`] to its declared source name through
    /// `id.name → Symbol → SymbolTable`.
    #[inline(always)]
    pub fn decl_name(&self, id: ItemKey) -> &str {
        &self.index.symbols[id.name]
    }

    /// Build a `{ generic_param_symbol → concrete_TypeId }` map for the
    /// instantiation carried by `recv_ty`. Returns `None` when the
    /// receiver isn't a generic instantiation (`TypeKind::Generic`) —
    /// i.e. there are no generic params to substitute.
    ///
    /// Looks up the receiver's owning HIR `TypeDecl` through
    /// [`ProjectIndex::locate_decl_in_ns`] to read the generic param
    /// symbols, then zips them with the receiver's concrete args. Used
    /// by capability handlers to render method signatures with the
    /// receiver's instantiation substituted (so hover / completion on
    /// `arr.add(...)` where `arr: Array<String>` shows
    /// `fn add(value: String): null`, not `fn add(value: T): null`).
    pub fn method_subst_from_receiver_ty(
        &self,
        recv_ty: TypeId,
    ) -> Option<FxHashMap<Symbol, TypeId>> {
        let (decl_id, args) = match &self.arena.get(recv_ty).kind {
            TypeKind::Generic { tpl, args } if !args.is_empty() => (*tpl, args.as_slice()),
            _ => return None,
        };
        let name_sym = decl_id.name;
        let (foreign_uri, foreign_decl_id) = self
            .index
            .locate_decl_in_ns(name_sym, crate::index::Namespace::Type)
            .next()?;
        let fmod = self.module(foreign_uri)?;
        let Decl::Type(td) = &fmod.hir.decls[foreign_decl_id] else {
            return None;
        };
        let mut subst: FxHashMap<Symbol, TypeId> = FxHashMap::default();
        for (i, gen_idx) in td.generics.iter().enumerate() {
            let gen_sym = fmod.hir.idents[*gen_idx].symbol;
            if let Some(arg_id) = args.get(i).copied() {
                subst.insert(gen_sym, arg_id);
            }
        }
        if subst.is_empty() {
            return None;
        }
        Some(subst)
    }

    /// One-pass build over every document currently in `manager`.
    pub fn analyze(manager: &SourceManager) -> Self {
        let mut out = Self::new();
        out.analyze_staged(manager);
        out
    }

    /// Staged analysis entrypoint.
    ///
    /// ```text
    /// S1   declare type/enum names         → type_id stable
    /// S2   declare fn names                → fn_id stable
    /// S3   declare modvar names            → modvar_id stable
    /// ─── all IDs stable ───
    /// S4   define type static-fields       (no types yet)
    /// S5   define type fields              (no types yet)
    /// S6   define type methods             (no params/return)
    /// ─── all type structure stable ───
    /// S7   complete type fields            (full TypeIds — monomorphize-ready)
    /// S8   complete type static-fields
    /// S9   complete type methods
    /// S10  complete fns
    /// S11  complete modvars
    /// ─── full structural typing knowledge ───
    /// S12  walk all bodies (CFG + narrowing + per-expr typing,
    ///                       monomorphize at call sites)
    /// ```
    pub fn analyze_staged(&mut self, manager: &SourceManager) {
        self.reset_state();

        let lowered = self.stage_lower(manager);
        self.extend_lint_rules(manager, &lowered);
        self.stage_lower_signatures(&lowered);
        self.stage_per_module_analysis(lowered);
        self.run_typed_lints(manager, None);
        self.validate_type_relations(None);
        self.compute_qualified_refs(manager);
        self.apply_rule_policy(None);
    }

    /// Applies the lint rules `@lint_off(...)` and `@lint_on(...)` from the `project.gcl` module
    /// globally to thye project analysis.
    fn extend_lint_rules(&mut self, manager: &SourceManager, lowered: &[LoweredModule]) {
        if let Some(entry_uri) = manager.entrypoint_uri() {
            for m in lowered {
                if &m.uri == entry_uri {
                    self.disabled_rules.extend(m.pragmas.off.iter().cloned());
                    self.enabled_rules.extend(m.pragmas.on.iter().cloned());
                    break;
                }
            }
        }
    }

    /// Drop every diagnostic whose rule is in `self.disabled_rules`
    /// (project-wide policy: CLI `--off` + the entrypoint's
    /// `@lint_off(...)`). Runs at the tail of `analyze_staged` and
    /// `invalidate` — the single source of truth for the disable
    /// side of the precedence stack.
    ///
    /// `restrict = Some(set)` limits the sweep to the listed URIs
    /// (matches the invalidate path); `None` sweeps every cached
    /// module.
    fn apply_rule_policy(&mut self, restrict: Option<&FxHashSet<&str>>) {
        let disabled = &self.disabled_rules;
        for (uri, module) in &mut self.modules {
            if let Some(set) = restrict
                && !set.contains(uri.as_str())
            {
                continue;
            }
            module.lints.retain(|l| !disabled.contains(l.rule));
        }
    }

    /// Reset every cached field so a `rebuild` / `analyze_staged`
    /// run starts from a known-empty state. Re-seeding builtins is
    /// idempotent (the arena interns them on the second insert).
    fn reset_state(&mut self) {
        self.modules.clear();
        self.arena = TypeArena::new(&self.index.symbols);
        let index = std::mem::take(&mut self.index);
        self.index = ProjectIndex::new(index.symbols, &self.arena);
        // Cached `TypeId`s reference the old arena, which
        // we just replaced. Drop the cache so the next
        // `lower_signatures_into` rebuilds against the fresh arena.
        self.sig_cache.clear();
    }

    /// **Stage S1** — parse + lower every module to HIR, ingest into
    /// the project index. Returns the lowered modules in document-order
    /// so [`Self::stage_per_module_analysis`] can move them into the
    /// per-module cache without re-lowering.
    fn stage_lower(&mut self, manager: &SourceManager) -> Vec<LoweredModule> {
        // Three phases. The middle one runs through the
        // `parallel` shim so native targets get rayon and wasm gets
        // a serial fallback; both branches live in `crate::parallel`.
        //
        // Phase A (serial): borrow each `RefCell<Document>` once and
        // extract the owned data the parallel phase needs. `Document`
        // is `!Sync` (it holds a tree-sitter `Parser` + a `OnceCell`),
        // so we can't hold its `Ref<'_, _>` across rayon's worker
        // boundaries. `Tree::clone` is reference-counted internally,
        // so the only real allocations here are the text + lib
        // strings — both bounded by total source size.
        // Capture the entrypoint URI now so the per-module
        // pragma walker can flip behavior when it's NOT the entrypoint.
        let entrypoint_uri: Option<&Uri> = manager.entrypoint_uri();

        struct ModuleToLower {
            uri: Uri,
            src: String,
            lib: String,
            tree: Tree,
            is_entry: bool,
        }
        let docs: Vec<ModuleToLower> = manager
            .iter()
            .map(|(uri, cell)| {
                let doc = cell.borrow();
                ModuleToLower {
                    uri: uri.clone(),
                    src: doc.text.clone(),
                    lib: doc.lib.clone(),
                    tree: doc.tree.clone(),
                    is_entry: entrypoint_uri == Some(uri),
                }
            })
            .collect();

        // Phase B (parallel on native, serial on wasm): lower each
        // module + parse its directives. No shared mutable state.
        let mut lowered = crate::parallel::par_map(docs, |m| {
            let lower_start = Instant::now();
            // Pass the real module name (filename minus `.gcl`) so
            // the HIR carries the module identity that `ingest` keys
            // decls by. Default `"module"` only kicks in for URIs
            // without a recognisable filename.
            let module_name = module_name_from_uri(&m.uri).unwrap_or("module");
            let hir = lower_module(
                &m.src,
                &self.index.symbols,
                module_name,
                m.lib.as_str(),
                m.tree.root_node(),
            );
            let lower_took = lower_start.elapsed();
            let directives = parse_directives(&m.src, m.tree.root_node());
            // Walk `@lint_off` / `@lint_on` annotations.
            // Entrypoint: collect rules + validate. Other modules: emit
            // `lint-pragma-outside-entrypoint` and discard the rules.
            let pragmas = parse_lint_pragmas(&m.src, m.tree.root_node(), m.is_entry);
            LoweredModule {
                uri: m.uri,
                hir,
                lib: m.lib,
                lower_took,
                directives,
                pragmas,
            }
        });

        // Phase C (serial): ingest into the project-wide index. This
        // mutates `self.index.symbols` etc., which is `!Send` on
        // purpose — it owns interner state that's amortised across
        // the whole project. `ingest` also mints decl handles into
        // `self.decl_registry` and allocates enum TypeIds into the
        // shared arena — folded in so every decl-registration step
        // happens in one place rather than spread across
        // `stage_lower` + `stage_lower_signatures`.
        //
        // Order matters: `ProjectIndex::ingest` is first-wins for
        // module-name claims (later files with the same stem land in
        // `duplicate_modules`). Sort so (a) the entrypoint always wins
        // its module slot — the user's own `project.gcl` should never
        // be flagged as a duplicate of a vendored `lib/foo/project.gcl`
        // — and (b) remaining ties resolve in deterministic URI order
        // so CI runs and local runs agree on which file gets the
        // `duplicate-module-name` diagnostic.
        lowered.sort_by(|a, b| {
            let a_is_entry = entrypoint_uri == Some(&a.uri);
            let b_is_entry = entrypoint_uri == Some(&b.uri);
            b_is_entry
                .cmp(&a_is_entry)
                .then_with(|| a.uri.as_str().cmp(b.uri.as_str()))
        });
        for m in &lowered {
            self.index
                .ingest(&m.uri, &m.hir, &mut self.arena, &mut self.decl_registry);
        }
        lowered
    }

    /// **Stages S7-S11** — lower every type's attr `TypeRef`s
    /// and method return-`TypeRef`s into the shared arena
    /// project-wide, then store the resulting `TypeId`s on each
    /// type's [`crate::index::TypeMembers`] entry.
    ///
    /// With these populated, the analyzer's per-module body walker
    /// can type cross-module `recv.attr` / `recv.method()` shapes
    /// inline by looking up `index.type_members[name].attr_types[prop]`
    /// and applying `arena.substitute(ty, &subst)` against the
    /// receiver's instantiation args (the type's own generics live in
    /// `TypeMembers::generics`).
    ///
    /// The lowering uses `ProjectIndex` for cross-module name
    /// resolution, so a foreign `Foo` resolves to `Named { name: "Foo" }`
    /// in the shared arena — directly comparable to anything else
    /// minted into the same arena. Generic params owned by the type
    /// being walked resolve to `GenericParam(T, owner=Type(name))`.
    fn stage_lower_signatures(&mut self, lowered: &[LoweredModule]) {
        let pairs: Vec<(&Uri, &Hir)> = lowered.iter().map(|m| (&m.uri, &m.hir)).collect();
        // `decl_registry` recording happens during
        // [`ProjectIndex::ingest`] now, so the signature-lowering pass
        // below sees a fully-populated registry from its first call.
        // (Previously this stage owned
        // a redundant decl pre-pass; the architectural rework folded
        // it into ingest so every decl-registration step lives in
        // one place and `enum_types` is populated before any method
        // return type that references an enum is lowered.)
        lower_signatures_into(
            &mut self.arena,
            &mut self.index,
            &self.decl_registry,
            &pairs,
            &mut self.sig_cache,
        );
    }
}

/// Fingerprint of the project-wide name set used by
/// [`lower_type_ref_project`].
/// We hash the names that *exist* (sorted, so the answer is order-
/// independent) so cached contributions can be reused only when the
/// flip outcome is identical to last time.
fn project_name_set_hash(index: &ProjectIndex) -> u64 {
    use std::collections::BTreeSet;

    // collect all: types + modvars + fn into a btreeset for order
    let ordered_names: BTreeSet<&str> = index
        .type_names
        .iter()
        .chain(&index.var_names)
        .chain(&index.fn_names)
        .map(|s| &index.symbols[*s])
        .collect();

    let mut hasher = DefaultHasher::new();
    for n in &ordered_names {
        n.hash(&mut hasher);
        // entry separator: defends against `["ab", "c"]` vs `["a", "bc"]` colliding.
        0u8.hash(&mut hasher);
    }
    hasher.finish()
}

/// Fingerprint of every byte
/// [`lower_module_signatures_walk`] would read out of `hir`. Walks
/// each top-level type / fn / enum decl name, generic ident text,
/// every reachable [`TypeRef`] (recursively), and the optional
/// marker on each ref. Body statements / expressions are skipped
/// they don't contribute to the project signature index.
fn module_signature_hash(hir: &Hir) -> u64 {
    let mut hasher = DefaultHasher::new();
    let Some(module) = hir.module.as_ref() else {
        0u8.hash(&mut hasher);
        return hasher.finish();
    };
    for d_id in &module.decls {
        match &hir.decls[*d_id] {
            Decl::Type(td) => {
                1u8.hash(&mut hasher);
                hir.idents[td.name].symbol.hash(&mut hasher);
                for g in &td.generics {
                    hir.idents[*g].symbol.hash(&mut hasher);
                }
                0u8.hash(&mut hasher);
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    hir.idents[attr.name].symbol.hash(&mut hasher);
                    if let Some(tr) = attr.ty {
                        hash_type_ref(&mut hasher, hir, tr);
                    } else {
                        0u8.hash(&mut hasher);
                    }
                }
                0u8.hash(&mut hasher);
                for method_id in &td.methods {
                    let Decl::Fn(fnd) = &hir.decls[*method_id] else {
                        continue;
                    };
                    hir.idents[fnd.name].symbol.hash(&mut hasher);
                    for g in &fnd.generics {
                        hir.idents[*g].symbol.hash(&mut hasher);
                    }
                    0u8.hash(&mut hasher);
                    if let Some(ret) = fnd.return_type {
                        hash_type_ref(&mut hasher, hir, ret);
                    } else {
                        0u8.hash(&mut hasher);
                    }
                }
                0u8.hash(&mut hasher);
            }
            Decl::Enum(ed) => {
                2u8.hash(&mut hasher);
                hir.idents[ed.name].symbol.hash(&mut hasher);
                for f in &ed.fields {
                    hir.idents[hir.enum_fields[*f].name]
                        .symbol
                        .hash(&mut hasher);
                }
                0u8.hash(&mut hasher);
            }
            Decl::Fn(fnd) => {
                3u8.hash(&mut hasher);
                hir.idents[fnd.name].symbol.hash(&mut hasher);
                for g in &fnd.generics {
                    hir.idents[*g].symbol.hash(&mut hasher);
                }
                0u8.hash(&mut hasher);
                if let Some(ret) = fnd.return_type {
                    hash_type_ref(&mut hasher, hir, ret);
                } else {
                    0u8.hash(&mut hasher);
                }
            }
            Decl::Var(vd) => {
                // Top-level vars contribute their declared
                // type to the project signature index, so the hash
                // must change when the var name or its TypeRef
                // shape changes.
                4u8.hash(&mut hasher);
                hir.idents[vd.name].symbol.hash(&mut hasher);
                if let Some(tr) = vd.ty {
                    hash_type_ref(&mut hasher, hir, tr);
                } else {
                    0u8.hash(&mut hasher);
                }
            }
            _ => {}
        }
    }
    hasher.finish()
}

fn hash_type_ref(hasher: &mut DefaultHasher, hir: &Hir, tr: Idx<TypeRef>) {
    let r = &hir.type_refs[tr];
    hir.idents[r.name].symbol.hash(hasher);
    r.optional.hash(hasher);
    for p in &r.params {
        hash_type_ref(hasher, hir, *p);
    }
    0u8.hash(hasher);
}

/// Free-function variant of [`ProjectAnalysis::stage_lower_signatures`]
/// that takes the arena + index as separate `&mut` borrows. Lets the
/// `invalidate` path build the `(Uri, &Hir)` slice from references
/// into `self.modules` without colliding with the `&mut self` recv
/// the method form would require.
///
/// When `cache` already has an entry for a module whose
/// `(sig_hash, name_set_hash)` pair matches the current state, the
/// cached contributions are reapplied verbatim instead of re-walking
/// the module. The arena is append-only across `invalidate`, so the
/// cached `TypeId`s remain comparable to anything minted in this
/// pass; on `analyze_staged` the cache is cleared by `reset_state`
/// so the rebuild walks every module fresh.
#[allow(clippy::mutable_key_type)]
fn lower_signatures_into(
    arena_mut: &mut TypeArena,
    index: &mut ProjectIndex,
    decl_registry: &DeclRegistry,
    lowered: &[(&Uri, &Hir)],
    cache: &mut FxHashMap<Uri, ModuleSigCache>,
) {
    let name_set_hash = project_name_set_hash(index);

    // First pass: drop cache entries for Uris that are no longer
    // present (a module was removed). Rebuilding ProjectIndex
    // from scratch in `invalidate` already drops their structural
    // entries, but the contributions cache needs explicit cleanup.
    let live_uris: FxHashSet<&Uri> = lowered.iter().map(|(u, _)| *u).collect();
    cache.retain(|u, _| live_uris.contains(u));

    // Second pass: per-module — reuse cache if hashes match,
    // otherwise re-walk and refresh the cache entry.
    for (uri, hir) in lowered {
        let sig_hash = module_signature_hash(hir);
        if let Some(c) = cache.get(*uri)
            && c.sig_hash == sig_hash
            && c.name_set_hash == name_set_hash
        {
            apply_module_contributions(index, c);
            continue;
        }
        let entry = lower_module_signatures(
            arena_mut,
            index,
            decl_registry,
            uri,
            hir,
            sig_hash,
            name_set_hash,
        );
        apply_module_contributions(index, &entry);
        cache.insert((*uri).clone(), entry);
    }

    // Post-pass: resolve each type's `supertype` field from its
    // source `extends` TypeRef to the parent's `ItemKey`. Deferred to
    // here because at ingest time the parent module may not have
    // been ingested yet (and ingest order is not topological).
    link_supertypes(index, lowered);
    // Post-pass: cache the deref-method return type on every type
    // whose decl carried `@deref("methodName")`. Runs after every
    // module's `method_returns` is in place so the lookup never
    // misses. The cached `TypeId` is still in the abstract
    // `GenericParam(T, …)` form — `arrow_deref_receiver` applies
    // the receiver's instantiation via `arena.substitute` at the
    // call site.
    populate_deref_caches(index);
    // P41.1
    populate_subtype_indices(index);
}

/// Walks every type decl across the project and patches each
/// [`TypeMembers::supertype`] field with the parent's resolved
/// [`ItemKey`]. Same-module supertypes are looked up directly; bare-
/// name cross-module supertypes filter `decl_locations` for the Type
/// namespace, skipping private decls (matches GreyCat's bare-name
/// visibility rule); qualified `mod::Super` supertypes route through
/// `module_names`. Primitives (`int`, `String`, …) are intentionally
/// dropped — they never form a `TypeMembers` entry to walk to.
#[allow(clippy::mutable_key_type)]
fn link_supertypes(index: &mut ProjectIndex, lowered: &[(&Uri, &Hir)]) {
    use crate::index::Namespace;
    use greycat_analyzer_hir::hir::Decl;

    // Two-pass to dodge the `&mut index` + `&index` aliasing the
    // resolution helper would otherwise need: first collect every
    // (self_id, parent_id) pair, then apply.
    let mut links: Vec<(ItemKey, ItemKey)> = Vec::new();
    for (uri, hir) in lowered {
        let Some(module) = hir.module.as_ref() else {
            continue;
        };
        let Some(stem) = module_name_from_uri(uri) else {
            continue;
        };
        let Some(module_sym) = index.symbols.lookup(stem) else {
            continue;
        };
        for decl_id in &module.decls {
            let Decl::Type(td) = &hir.decls[*decl_id] else {
                continue;
            };
            let Some(super_tr) = td.supertype else {
                continue;
            };
            let parent_ref = &hir.type_refs[super_tr];
            let parent_name = hir.idents[parent_ref.name].symbol;
            let parent_id = if let Some(last) = parent_ref.qualifier.last() {
                // Qualified `mod::Super`: the qualifier's last segment names the owning module.
                let qual_sym = hir.idents[*last].symbol;
                let Some(qual_uri) = index.module_names.get(&qual_sym) else {
                    continue;
                };
                let Some(qual_stem) = module_name_from_uri(qual_uri) else {
                    continue;
                };
                let parent_module = index.symbols.intern(qual_stem);
                ItemKey::new(parent_module, parent_name)
            } else {
                // Bare `Super` — same-module wins first, then cross-
                // module via the resolver's name-set (filtered to
                // non-private Type-namespace candidates).
                let local = ItemKey::new(module_sym, parent_name);
                if index.type_members.contains_key(&local) {
                    local
                } else {
                    let mut hit: Option<ItemKey> = None;
                    for (cand_uri, decl) in index.locate_decl_in_ns(parent_name, Namespace::Type) {
                        if index.is_decl_private(cand_uri, decl) {
                            continue;
                        }
                        let Some(cand_stem) = module_name_from_uri(cand_uri) else {
                            continue;
                        };
                        let cand_module = index.symbols.intern(cand_stem);
                        hit = Some(ItemKey::new(cand_module, parent_name));
                        break;
                    }
                    let Some(found) = hit else { continue };
                    found
                }
            };
            let self_id = ItemKey::new(module_sym, hir.idents[td.name].symbol);
            links.push((self_id, parent_id));
        }
    }
    for (self_id, parent_id) in links {
        if let Some(tm) = index.type_members.get_mut(&self_id) {
            tm.supertype = Some(parent_id);
        }
    }
}

// P41.1
/// Build `subtype_closure` (every type → canonical-sorted concrete
/// leaves) and `abstract_by_closure_set` (reverse index mapping each
/// abstract's closure to its name, for the mandatory ancestor-
/// collapse in `narrow_complement`).
///
/// Algorithm:
/// 1. Invert `type_members[*].supertype` into a direct-child map.
/// 2. Memoized DFS: `closure(X) = ({X} if X concrete) ∪ ⋃ closure(child)`.
/// 3. Reverse index: for each abstract `A`, insert `closure(A) → A`.
///    Iterate abstracts in Symbol-alpha order (by their resolved
///    name text) so collisions resolve deterministically across
///    re-lowers — first-inserted wins, and that's always the
///    alphabetically-earlier name.
///
/// Closure entries are stored canonically sorted by `Symbol`'s `Ord`
/// impl so the reverse-index `get(...)` is order-independent at the
/// call site.
fn populate_subtype_indices(index: &mut ProjectIndex) {
    use rustc_hash::FxHashSet;

    // Snapshot every type ItemKey we'll need to compute closures for.
    let all_types: Vec<ItemKey> = index.type_members.keys().copied().collect();

    // Step 1: invert supertype to direct-child.
    let mut children: FxHashMap<ItemKey, Vec<ItemKey>> = FxHashMap::default();
    for (id, members) in &index.type_members {
        if let Some(parent) = members.supertype {
            children.entry(parent).or_default().push(*id);
        }
    }

    // Step 2: memoized closure build. Recursive helper; `memo` is
    // shared across roots so sibling subtrees don't redo work.
    fn build(
        id: ItemKey,
        children: &FxHashMap<ItemKey, Vec<ItemKey>>,
        is_abstract: &FxHashSet<ItemKey>,
        memo: &mut FxHashMap<ItemKey, Box<[ItemKey]>>,
    ) {
        if memo.contains_key(&id) {
            return;
        }
        // Sentinel insert defends against accidental cycles in
        // `supertype` (shouldn't occur, but corrupt fixtures or
        // half-loaded projects could otherwise loop here).
        memo.insert(id, Box::default());
        let mut set: FxHashSet<ItemKey> = FxHashSet::default();
        if !is_abstract.contains(&id) {
            set.insert(id);
        }
        if let Some(ch) = children.get(&id) {
            for c in ch.clone() {
                build(c, children, is_abstract, memo);
                if let Some(c_closure) = memo.get(&c) {
                    set.extend(c_closure.iter().copied());
                }
            }
        }
        let mut sorted: Vec<ItemKey> = set.into_iter().collect();
        sorted.sort();
        memo.insert(id, sorted.into_boxed_slice());
    }

    let mut closure: FxHashMap<ItemKey, Box<[ItemKey]>> = FxHashMap::default();
    for id in &all_types {
        build(*id, &children, &index.is_abstract, &mut closure);
    }

    // Step 3: reverse index, abstracts only, ordered alphabetically
    // by (module-name, item-name) text so collisions resolve
    // deterministically across re-lowers. `Symbol::Ord` is u32-order
    // (intern-timing dependent), so resolve back through the symbol
    // table for stable string-order comparison.
    let mut abstract_ids: Vec<ItemKey> = index.is_abstract.iter().copied().collect();
    abstract_ids.sort_by(|a, b| {
        (&index.symbols[a.module], &index.symbols[a.name])
            .cmp(&(&index.symbols[b.module], &index.symbols[b.name]))
    });
    let mut reverse: FxHashMap<Box<[ItemKey]>, ItemKey> = FxHashMap::default();
    for id in abstract_ids {
        if let Some(c) = closure.get(&id) {
            reverse.entry(c.clone()).or_insert(id);
        }
    }

    index.subtype_closure = closure;
    index.abstract_by_closure_set = reverse;
}

fn populate_deref_caches(index: &mut ProjectIndex) {
    use rustc_hash::FxHashMap;
    // Two-pass: build a snapshot of (type_id → ret_ty) pairs from
    // `type_flags`, then write back. Avoids holding `&type_members`
    // + `&mut type_members` borrows simultaneously. With `type_flags`
    // now keyed by `ItemKey`, each entry maps 1:1 to its type_members
    // entry — no name-match scan needed.
    let mut resolutions: FxHashMap<ItemKey, TypeId> = FxHashMap::default();
    for (type_id, flags) in &index.type_flags {
        let Some(method_name) = flags.deref else {
            continue;
        };
        let name = &index.symbols[method_name];
        if name.is_empty() {
            continue;
        }
        if let Some(ret) = index.type_method_return_chain(*type_id, method_name) {
            resolutions.insert(*type_id, ret);
        }
    }
    for (type_id, ret_ty) in resolutions {
        if let Some(tm) = index.type_members.get_mut(&type_id) {
            tm.deref_return_ty = Some(ret_ty);
        }
    }
}

/// Walk a single module's signatures and return the
/// contributions it would write into the project index. Mutates
/// `index.symbols` in passing (every contributed name is interned
/// so cache entries can use `Symbol` keys).
fn lower_module_signatures(
    arena: &mut TypeArena,
    index: &mut ProjectIndex,
    decl_registry: &DeclRegistry,
    uri: &Uri,
    hir: &Hir,
    sig_hash: u64,
    name_set_hash: u64,
) -> ModuleSigCache {
    let mut entry = ModuleSigCache {
        sig_hash,
        name_set_hash,
        ..Default::default()
    };
    let Some(module) = hir.module.as_ref() else {
        return entry;
    };
    for d_id in &module.decls {
        match &hir.decls[*d_id] {
            Decl::Type(td) => {
                let type_sym = hir.idents[td.name].symbol;
                let Some(type_id) = index.item_id_for(uri, type_sym) else {
                    continue;
                };
                let owner = GenericOwner::Type(type_sym);
                let mut generics_in_scope: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                for g in &td.generics {
                    let g_sym = hir.idents[*g].symbol;
                    generics_in_scope.insert(g_sym, owner);
                }
                if let Some(super_tr) = td.supertype {
                    // Lower the supertype's TypeRef with the type's
                    // own generic params in scope so a `Sub extends
                    // Base<T>` for a generic `Sub<T>` lands as
                    // `Generic { decl: Base, args: [GenericParam(T,
                    // owner=Sub)] }`.
                    let ty = lower_type_ref_project(
                        hir,
                        super_tr,
                        arena,
                        &*index,
                        decl_registry,
                        &generics_in_scope,
                        Some(uri),
                    );
                    entry.supertypes.push((type_id, ty));
                }
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    let attr_sym = hir.idents[attr.name].symbol;
                    let Some(tr) = attr.ty else {
                        continue;
                    };
                    let ty = lower_type_ref_project(
                        hir,
                        tr,
                        arena,
                        &*index,
                        decl_registry,
                        &generics_in_scope,
                        Some(uri),
                    );
                    entry.attrs.push((type_id, attr_sym, ty));
                }
                for method_id in &td.methods {
                    let Decl::Fn(fnd) = &hir.decls[*method_id] else {
                        continue;
                    };
                    let method_sym = hir.idents[fnd.name].symbol;
                    // P19.8: push the method's generics onto the
                    // type-level scope, lower, then pop. Avoids
                    // cloning `generics_in_scope` (a HashMap with
                    // GenericOwner-owned Strings) per method —
                    // overrides of the outer scope are saved and
                    // restored.
                    let method_owner = GenericOwner::Function(method_sym);
                    let mut saved: Vec<(Symbol, Option<GenericOwner>)> =
                        Vec::with_capacity(fnd.generics.len());
                    for g in &fnd.generics {
                        let g_sym = hir.idents[*g].symbol;
                        let prev = generics_in_scope.insert(g_sym, method_owner);
                        saved.push((g_sym, prev));
                    }
                    if let Some(ret) = fnd.return_type {
                        let ty = lower_type_ref_project(
                            hir,
                            ret,
                            arena,
                            &*index,
                            decl_registry,
                            &generics_in_scope,
                            Some(uri),
                        );
                        entry.methods.push((type_id, method_sym, ty));
                    }
                    // Pre-lower the full method signature for every
                    // method, generic or not. Generic methods need it
                    // for call-site inference (`run_method_generic_inference`);
                    // non-generic ones need it for the lambda-unify
                    // `fn_ref_ty_from_sig` helper to mint a structural
                    // Lambda when a static method is referenced in
                    // value position (`Runtime::on_files_put`).
                    let method_generics: Vec<Symbol> =
                        fnd.generics.iter().map(|g| hir.idents[*g].symbol).collect();
                    let mut method_params: Vec<TypeId> = Vec::with_capacity(fnd.params.len());
                    for p_id in &fnd.params {
                        let p = &hir.fn_params[*p_id];
                        let pt = if let Some(tr) = p.ty {
                            lower_type_ref_project(
                                hir,
                                tr,
                                arena,
                                &*index,
                                decl_registry,
                                &generics_in_scope,
                                Some(uri),
                            )
                        } else {
                            arena.any_nullable()
                        };
                        method_params.push(pt);
                    }
                    // `return_ty: None` when the method declares no
                    // return type — preserves the "no observable
                    // return" semantic for the structural-Lambda
                    // mint downstream. Call-typing consumers fall
                    // back to `any?` at their use site.
                    let method_ret_ty = fnd.return_type.map(|ret| {
                        lower_type_ref_project(
                            hir,
                            ret,
                            arena,
                            &*index,
                            decl_registry,
                            &generics_in_scope,
                            Some(uri),
                        )
                    });
                    entry.method_sigs.push((
                        type_id,
                        method_sym,
                        FnSignature {
                            home_uri: uri.clone(),
                            return_ty: method_ret_ty,
                            generics: method_generics,
                            params: method_params,
                            return_erases: crate::erasure::fn_result_erases(hir, fnd),
                        },
                    ));
                    // Restore: undo every push (in reverse so a method
                    // that re-shadows an outer name plays back correctly).
                    for (k, prev) in saved.into_iter().rev() {
                        match prev {
                            Some(v) => {
                                generics_in_scope.insert(k, v);
                            }
                            None => {
                                generics_in_scope.remove(&k);
                            }
                        }
                    }
                }
            }
            Decl::Enum(_) => {
                // No-op: enum TypeIds are minted by
                // [`ProjectIndex::ingest`] now and published into
                // `index.enum_types` immediately, so the signature
                // pass has nothing to do here. The previous
                // implementation re-allocated the same Enum into
                // the shared arena and pushed to the cache; the
                // intern-by-value behaviour made it redundant
                // *and* it ran in source order, leaving
                // `enum_type_for(name)` returning `None` for any
                // method signature lowered earlier in the same
                // module that referenced the enum — surfacing as
                // a `T not assignable to T` regression at the
                // validation stage.
            }
            Decl::Fn(fnd) => {
                let fn_sym = hir.idents[fnd.name].symbol;
                let Some(fn_id) = index.item_id_for(uri, fn_sym) else {
                    continue;
                };
                let Some(ret) = fnd.return_type else {
                    continue;
                };
                let owner = GenericOwner::Function(fn_sym);
                let mut generics_in_scope: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                let mut generics: Vec<Symbol> = Vec::with_capacity(fnd.generics.len());
                for g in &fnd.generics {
                    let g_sym = hir.idents[*g].symbol;
                    generics_in_scope.insert(g_sym, owner);
                    generics.push(g_sym);
                }
                let ret_ty = lower_type_ref_project(
                    hir,
                    ret,
                    arena,
                    &*index,
                    decl_registry,
                    &generics_in_scope,
                    Some(uri),
                );
                // **P19.15** — also pre-lower parameter types so the
                // analyzer's generic-call inference can run on
                // cross-module callees (`abs`, `min`, `max`, …).
                let mut params: Vec<TypeId> = Vec::with_capacity(fnd.params.len());
                for p_id in &fnd.params {
                    let p = &hir.fn_params[*p_id];
                    let pt = if let Some(tr) = p.ty {
                        lower_type_ref_project(
                            hir,
                            tr,
                            arena,
                            &*index,
                            decl_registry,
                            &generics_in_scope,
                            Some(uri),
                        )
                    } else {
                        arena.any_nullable()
                    };
                    params.push(pt);
                }
                entry.fns.push((
                    fn_id,
                    FnSignature {
                        home_uri: uri.clone(),
                        return_ty: Some(ret_ty),
                        generics,
                        params,
                        return_erases: crate::erasure::fn_result_erases(hir, fnd),
                    },
                ));
            }
            Decl::Var(vd) => {
                // P19.10 — pre-lower top-level var declared types
                // into the shared arena so a cross-module bare
                // reference (`Definition::ProjectDecl` pointing at
                // a `Decl::Var`) can pull the real type out of
                // `index.var_types` instead of falling through to
                // `Named("type")`. Vars without a declared type
                // contribute nothing — the analyzer's local body
                // walker types them from the initializer.
                let var_sym = hir.idents[vd.name].symbol;
                let Some(var_id) = index.item_id_for(uri, var_sym) else {
                    continue;
                };
                let Some(tr) = vd.ty else {
                    continue;
                };
                // Vars never declare generics, so no scope needed.
                let empty: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                let var_ty = lower_type_ref_project(
                    hir,
                    tr,
                    arena,
                    &*index,
                    decl_registry,
                    &empty,
                    Some(uri),
                );
                entry.vars.push((var_id, var_ty));
            }
            _ => {}
        }
    }
    entry
}

/// Index-aware extension of [`is_assignable_to`]
/// that recognises user-declared inheritance. Adds two cases on top
/// of the standard relation:
/// - `Named(Sub)` is assignable to `Named(Super)` when `Sub` is `Super`'s
///   transitive descendant in `index.type_members[*].supertype`.
/// - `Generic("node", [Sub])` is assignable to `Generic("node", [Super])`
///   under the same chain. Other generics (`Array`, `Map`, …) stay
///   invariant — the runtime treats `node<T>` as covariant in `T` for
///   subtyping but rejects covariance on container generics.
///
/// Falls back to the standard relation when neither case applies, so
/// every primitive / nullable / lambda / tuple rule still fires.
pub(crate) fn is_assignable_to_with_index(
    index: &ProjectIndex,
    _decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    from: TypeId,
    to: TypeId,
) -> bool {
    if arena.is_assignable_to(from, to) {
        return true;
    }
    let a_nullable = arena.get(from).nullable;
    let b_nullable = arena.get(to).nullable;
    if a_nullable && !b_nullable {
        return false;
    }
    // Clone kinds upfront so the immutable arena borrow ends before
    // any recursive call needs `&mut arena` (substitute hops in the
    // generic-supertype walks allocate fresh `Generic` nodes).
    let a_kind = arena.get(from).kind.clone();
    let b_kind = arena.get(to).kind.clone();
    // Exhaustive nested match. Same rationale as core's
    // `is_assignable_to`: a `_ => false` would absorb future
    // `TypeKind` variants and re-introduce the `Union → supertype`
    // class of bug at this layer. Each source-arm exhaustively
    // handles every target-kind; cases the wrapper doesn't extend
    // beyond core (already tried above) return `false` explicitly.
    //
    // Union arms recurse into `_with_index` (not core) so each
    // per-alt check picks up the supertype chain. Source-Union
    // fires first; target-Union recurses against each alt.
    match a_kind {
        // Caught by the `is_assignable_to(...)` early-return above:
        // these source kinds short-circuit in core (Null via target
        // nullability; Never/Any/Unresolved as top/bottom rules).
        TypeKind::Null | TypeKind::Any | TypeKind::Never | TypeKind::Unresolved { .. } => false,

        TypeKind::Union { alts } => alts
            .into_iter()
            .all(|alt| is_assignable_to_with_index(index, _decl_registry, arena, alt, to)),

        // P-typeof — type-literal source. Accepts `Type(core::type)`
        // as a widening target so stdlib functions typed `(t: type)`
        // continue to accept arguments now typed as `TypeOf(X)`.
        // Identity TypeOf → TypeOf is handled by the core fast path.
        TypeKind::TypeOf(_) => match b_kind {
            TypeKind::Type(d) => d == arena.builtins.type_key,
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_assignable_to_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Handle-keyed subtype chain — the inheritance layer this
        // wrapper exists to add. `Type(sub) → Type(sup)` when `sub`
        // is a transitive descendant of `sup` in `index.type_members`.
        TypeKind::Type(sub) => match b_kind {
            TypeKind::Type(sup) => index.is_subtype_of_decl(sub, sup),
            // `Type(sub) → Generic(sup<args>)` — `sub` is a non-
            // generic concrete type whose `extends` chain reaches the
            // generic shape on the right (e.g. `PointChangeView
            // extends GridChangeView<Point>` being passed where
            // `GridChangeView<any?>` is expected). Walk the chain,
            // looking for a hop whose pre-lowered `supertype_ty` is
            // a `Generic { decl: sup_decl, .. }` instantiation, then
            // hand the result to core's invariance / all-Any wildcard
            // check. No substitution needed at this layer — `sub`
            // is non-generic, so its parent's args are already
            // fully concrete.
            TypeKind::Generic { .. } => {
                walk_substituted_supertype_chain(index, arena, sub, &[], |arena, hop| {
                    arena.is_assignable_to(hop, to)
                })
            }
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_assignable_to_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Node-tag bivariance + node<T> covariance + cross-decl
        // generic supertype walk. Same-decl generics stay invariant
        // (handled by core's same-handle args check).
        TypeKind::Generic { tpl: da, args: aa } => match b_kind {
            TypeKind::Generic {
                tpl: db,
                args: ref ab,
            } if da == db && aa.len() == ab.len() && arena.is_node_tag(da) => true,
            TypeKind::Generic {
                tpl: db,
                args: ref ab,
            } if da == db && aa.len() == 1 && ab.len() == 1 && arena.is_node(da) => {
                // node<Sub> -> node<Super> when Sub extends Super.
                // Recurse so a chain like node<DeepSub> ->
                // node<MidSub> -> node<Super> works in one hop.
                let (a0, b0) = (aa[0], ab[0]);
                is_assignable_to_with_index(index, _decl_registry, arena, a0, b0)
            }
            // Cross-decl generic source: walk `da`'s pre-lowered
            // supertype_ty chain, substituting `aa` for `da`'s own
            // `GenericParam` slots at each hop, then check the
            // substituted shape against `to`. Covers
            // `MultiQuantizer<T> extends Quantizer<Array<T>>` —
            // passing `MultiQuantizer<int>` to a parameter typed
            // `Quantizer<Array<int>>`.
            TypeKind::Generic { .. } => {
                walk_substituted_supertype_chain(index, arena, da, &aa, |arena, hop| {
                    arena.is_assignable_to(hop, to)
                })
            }
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_assignable_to_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Type(_)
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Lambda source: structural→nominal `fn(...) -> Type(function)`.
        // Any lambda flows into the opaque GCL `function` slot — the
        // dual of "lambdas and `function` are two concepts unified at
        // the type-checker." Target-Union retry as for the sibling
        // kinds below. The reverse direction (function → specific
        // Lambda{...}) is intentionally NOT added: the opaque side
        // carries no signature, so admitting it into a typed slot
        // would be unsound.
        TypeKind::Lambda { .. } => match b_kind {
            TypeKind::Type(d) if d == arena.builtins.function_key => true,
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_assignable_to_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Enum / GenericParam: the wrapper adds no inheritance-aware
        // rules beyond what core already covers. Only the target-Union
        // retry is meaningful (a single alt might match via the
        // wrapper's extensions even when core rejected the whole union).
        // Primitives are `Type(core::X)` and flow through the `Type(sub)`
        // arm above (identity + target-Union retry, no supertypes).
        TypeKind::Enum { .. } | TypeKind::GenericParam { .. } => match b_kind {
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_assignable_to_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },
    }
}

/// Walk `sub_decl`'s pre-lowered `supertype_ty` chain, substituting
/// `sub_args` for `sub_decl`'s own `GenericParam` slots at each hop,
/// and call `on_hop` with each hop's substituted `TypeId`. Returns
/// the first `true` from `on_hop`; returns `false` if the chain
/// exhausts without a match (or if the budget trips).
///
/// Handles the non-generic-source case uniformly: a decl with zero
/// generics produces an empty substitution map, so `arena.substitute`
/// is a no-op and the walk proceeds the same way. Pass `sub_args = &[]`.
///
/// Shared by assignability and cast — each caller supplies its own
/// per-hop predicate (assignability uses core `is_assignable_to`; cast
/// uses a bidirectional per-arg cast-compat check). Hop budget (32)
/// mirrors `is_subtype_of` — deeper chains would have stack-overflowed
/// in dependent passes already.
fn walk_substituted_supertype_chain<F>(
    index: &ProjectIndex,
    arena: &mut TypeArena,
    sub_decl: ItemKey,
    sub_args: &[TypeId],
    mut on_hop: F,
) -> bool
where
    F: FnMut(&mut TypeArena, TypeId) -> bool,
{
    let mut current_tpl = sub_decl;
    let mut current_args: Vec<TypeId> = sub_args.to_vec();
    for _ in 0..32 {
        let Some(members) = index.type_members.get(&current_tpl) else {
            return false;
        };
        if members.generics.len() != current_args.len() {
            return false;
        }
        let Some(sup_ty_raw) = members.supertype_ty else {
            return false;
        };
        let subst: FxHashMap<Symbol, TypeId> = members
            .generics
            .iter()
            .copied()
            .zip(current_args.iter().copied())
            .collect();
        let sup_ty = arena.substitute(sup_ty_raw, &subst);
        if on_hop(arena, sup_ty) {
            return true;
        }
        match arena.get(sup_ty).kind.clone() {
            TypeKind::Generic { tpl, args } => {
                current_tpl = tpl;
                current_args = args.to_vec();
            }
            TypeKind::Type(d) => {
                current_tpl = d;
                current_args.clear();
            }
            _ => return false,
        }
    }
    false
}

/// Index-aware extension of [`is_castable`].
/// Adds the symmetric node-tag-handle / int cast rules — handles are
/// 64-bit ints at runtime, so `nodeTime<T> as int` and `int as
/// nodeTime<T>` both succeed. Dispatch via `TypeArena::is_node_tag
/// (decl)` so a user-declared `type node<T>` (which has its own
/// handle) doesn't accidentally pick up these rules.
///
/// Inheritance-aware: cross-decl generic casts (`Sub<T> as Sup<F<T>>`)
/// go through the shared [`walk_substituted_supertype_chain`] with a
/// cast-style per-hop predicate, so the wrapper rejects obviously-wrong
/// casts like `MultiQuantizer<int> as Quantizer<Array<String>>`. The
/// runtime drops `as` casts entirely — the analyzer is the only safety
/// net, so generic-arg strictness here matches assignability's.
pub(crate) fn is_castable_with_index(
    index: &ProjectIndex,
    _decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    from: TypeId,
    to: TypeId,
) -> bool {
    if arena.is_castable(from, to) {
        return true;
    }
    // Clone kinds upfront so the immutable arena borrow ends before
    // any recursive call needs `&mut arena` (the chain walker
    // substitutes, which allocates fresh `Generic` nodes).
    let from_kind = arena.get(from).kind.clone();
    let to_kind = arena.get(to).kind.clone();
    // Node-tag <-> int cast bivariance. `int` is `Type(core::int)`, so
    // this is the single place the rule lives -- neither side's structural
    // arm below sees it.
    let from_node_tag =
        matches!(&from_kind, TypeKind::Generic { tpl, .. } if arena.is_node_tag(*tpl));
    let to_node_tag = matches!(&to_kind, TypeKind::Generic { tpl, .. } if arena.is_node_tag(*tpl));
    if (from == arena.builtins.int && to_node_tag) || (from_node_tag && to == arena.builtins.int) {
        return true;
    }
    // Exhaustive nested match. Same rationale as the assignability
    // wrapper: no `_ => false` so future `TypeKind` variants can't
    // silently slip past inheritance / node-tag rules. Union arms
    // recurse into self so each alt picks up everything below.
    match from_kind {
        // Caught by the `is_castable(...)` early-return above.
        TypeKind::Null | TypeKind::Any | TypeKind::Never | TypeKind::Unresolved { .. } => false,

        // Union source: mirror core's `.any()` semantics — `as` is a
        // downcast intent, so the wrapper's inheritance-aware extension
        // also accepts the union when AT LEAST ONE alt could possibly
        // cast. Switching from `.all()` to `.any()` here keeps the two
        // layers in lockstep; otherwise the inheritance-aware retry
        // would silently undo core's fix.
        TypeKind::Union { alts } => alts
            .into_iter()
            .any(|alt| is_castable_with_index(index, _decl_registry, arena, alt, to)),

        // P-typeof — `as` is dropped at runtime; the analyzer mirrors
        // assignability for cast strictness, so accept the widening
        // `TypeOf(X) → Type(core::type)` for the same reason. Other
        // targets reject (identity goes through the early-return).
        TypeKind::TypeOf(_) => match to_kind {
            TypeKind::Type(d) => d == arena.builtins.type_key,
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_castable_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Source Type: bidirectional inheritance with Type / Generic
        // targets; target-Union retry. No node-tag-int rule applies
        // (Type is non-generic, can't be a node tag). Non-generic
        // source has no args to substitute — the cross-decl arg
        // strictness only kicks in when both sides are Generic.
        TypeKind::Type(fd) => match to_kind {
            TypeKind::Type(td) => {
                index.is_subtype_of_decl(fd, td) || index.is_subtype_of_decl(td, fd)
            }
            TypeKind::Generic { tpl: td, .. } => {
                // Upcast (Sub:Type → Sup<args>): walk Sub's chain
                // looking for a hop whose decl matches Sup; that hop
                // is already in fully concrete form, so use core's
                // `is_castable` per-hop.
                if index.is_subtype_of_decl(fd, td) {
                    walk_substituted_supertype_chain(index, arena, fd, &[], |arena, hop| {
                        arena.is_castable(hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast (Sup → Sub<args>): walk Sub (the more-
                    // specific side) with its concrete args, find a
                    // hop matching Sup. Source is non-generic, so the
                    // hop must equal `from` for the cast to make sense.
                    // Defer to core via `is_castable(hop, from)`.
                    let to_decl_args = match &arena.get(to).kind {
                        TypeKind::Generic { args, .. } => args.to_vec(),
                        _ => return false,
                    };
                    walk_substituted_supertype_chain(
                        index,
                        arena,
                        td,
                        &to_decl_args,
                        |arena, hop| arena.is_castable(hop, from),
                    )
                } else {
                    false
                }
            }
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_castable_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Source Generic: same-decl trusts core (which has already
        // rejected before we got here, so further wrapper work means
        // mismatched args — except node-tag bivariance, mirroring
        // assignability). Different decls: walk the more-specific
        // side's chain with substituted args, per-hop compare via
        // core `is_castable` so args are checked invariantly.
        // `<node-tag> as int` succeeds because the runtime handle is
        // a 64-bit int.
        TypeKind::Generic { tpl: fd, args: fa } => match to_kind {
            TypeKind::Type(td) => {
                if index.is_subtype_of_decl(fd, td) {
                    // Upcast (Sub<args>:Generic → Sup:Type): walk
                    // Sub's chain with its args; hop matching Sup
                    // (non-generic) is the destination.
                    walk_substituted_supertype_chain(index, arena, fd, &fa, |arena, hop| {
                        arena.is_castable(hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast: target is non-generic but its decl is
                    // a subtype of source's decl. Walk target's chain
                    // (no args), per-hop check against source.
                    walk_substituted_supertype_chain(index, arena, td, &[], |arena, hop| {
                        arena.is_castable(hop, from)
                    })
                } else {
                    false
                }
            }
            TypeKind::Generic {
                tpl: td,
                args: ref ta,
            } => {
                if fd == td {
                    // Same decl: core's `is_castable` already gave its
                    // verdict (false, otherwise the early-return fired).
                    // Wrapper-side relaxation: node-tag bivariance —
                    // mirrors `is_assignable_to_with_index` so the two
                    // layers agree on what's allowed for tag args.
                    fa.len() == ta.len() && arena.is_node_tag(fd)
                } else if index.is_subtype_of_decl(fd, td) {
                    // Upcast: walk source's chain with source's args.
                    walk_substituted_supertype_chain(index, arena, fd, &fa, |arena, hop| {
                        arena.is_castable(hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast: walk target's chain with target's args,
                    // per-hop compare against source. Args are checked
                    // invariantly by core's `is_castable`.
                    let ta_owned = ta.to_vec();
                    walk_substituted_supertype_chain(index, arena, td, &ta_owned, |arena, hop| {
                        arena.is_castable(hop, from)
                    })
                } else {
                    false
                }
            }
            TypeKind::Union { alts } => alts
                .into_iter()
                .any(|alt| is_castable_with_index(index, _decl_registry, arena, from, alt)),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Other source kinds: wrapper adds no rules beyond core; only
        // the target-Union retry might matter (an alt could pick up
        // a wrapper rule even when the whole union didn't). The node-tag
        // <-> int cast bivariance (for primitive `Type(core::int)`) is
        // handled by the top-level block at the head of this function.
        TypeKind::Lambda { .. } | TypeKind::Enum { .. } | TypeKind::GenericParam { .. } => {
            match to_kind {
                TypeKind::Union { alts } => alts
                    .into_iter()
                    .any(|alt| is_castable_with_index(index, _decl_registry, arena, from, alt)),
                TypeKind::Null
                | TypeKind::Any
                | TypeKind::Never
                | TypeKind::Unresolved { .. }
                | TypeKind::Type(_)
                | TypeKind::Generic { .. }
                | TypeKind::Lambda { .. }
                | TypeKind::Enum { .. }
                | TypeKind::GenericParam { .. }
                | TypeKind::TypeOf(_) => false,
            }
        }
    }
}

/// Apply a cached / freshly-built module contribution to
/// the project index. Mirrors the apply-loop the original
/// `lower_signatures_into` ran at end-of-pass: `or_insert` semantics
/// preserve the "first decl wins" collision rule that the rest of
/// the pipeline assumes.
fn apply_module_contributions(index: &mut ProjectIndex, c: &ModuleSigCache) {
    for (type_id, attr_sym, ty) in &c.attrs {
        if let Some(tm) = index.type_members.get_mut(type_id) {
            tm.attr_types.insert(*attr_sym, *ty);
        }
    }
    for (type_id, method_sym, ty) in &c.methods {
        if let Some(tm) = index.type_members.get_mut(type_id) {
            tm.method_returns.insert(*method_sym, *ty);
        }
    }
    for (type_id, method_sym, sig) in &c.method_sigs {
        if let Some(tm) = index.type_members.get_mut(type_id) {
            tm.method_signatures
                .entry(*method_sym)
                .or_insert_with(|| sig.clone());
        }
    }
    for (fn_id, sig) in &c.fns {
        index
            .fn_signatures
            .entry(*fn_id)
            .or_insert_with(|| sig.clone());
    }
    for (var_id, ty) in &c.vars {
        index.var_types.entry(*var_id).or_insert(*ty);
    }
    for (type_id, ty) in &c.supertypes {
        if let Some(tm) = index.type_members.get_mut(type_id)
            && tm.supertype_ty.is_none()
        {
            tm.supertype_ty = Some(*ty);
        }
    }
}

impl ProjectAnalysis {
    /// **Stages S2-S11 + S12 (per-module slice).** Currently delegates
    /// to `analyze_with_index_into`, which combines name declaration,
    /// structure declaration, signature lowering, and body walking
    /// inside `Cx::visit_decl`. Subsequent extraction passes will split
    /// out S2-S6, S7-S11, and S12 — at which point this stage shrinks
    /// to a thin "wire it all together" call.
    fn stage_per_module_analysis(&mut self, hirs: Vec<LoweredModule>) {
        let bypass = self.bypass_suppressions;
        let index = &self.index;
        // The std-core `Map` identity lets the resolver treat
        // `Map { k: v }` keys as value expressions. `Copy`, so the
        // parallel pass-A closure captures it by value.
        let map_decl = Some(self.arena.builtins.map_key);

        // Split the per-module pass into two phases:
        //
        //   Pass A (parallel): resolve + HIR-shape lints. Both are
        //   read-only against `&self.index` and write only to their
        //   per-module return values.
        //
        //   Pass B (serial): the analyzer's body walker
        //   (`analyze_with_index_into`), which mutates `&mut self.arena`
        //   for type allocation. P26.5 will probe whether wrapping
        //   the arena in a Mutex makes this parallel too; for now it
        //   stays serial.
        //
        // The serial path under `cfg(not(feature = "parallel"))` runs
        // both phases inline per module, identical to the original
        // sequential flow.
        struct PassAOut {
            uri: Uri,
            hir: Hir,
            lib: String,
            lower_took: Duration,
            directives: Directives,
            resolutions: Resolutions,
            lints: Vec<LintDiagnostic>,
            resolve_took: Duration,
            lint_took: Duration,
        }

        let pass_a_run = |LoweredModule {
                              uri,
                              hir,
                              lib,
                              lower_took,
                              mut directives,
                              mut pragmas,
                          }|
         -> PassAOut {
            let t0 = Instant::now();
            let resolutions = self.index.resolutions(&hir, Some(&uri), map_decl);
            let resolve_took = t0.elapsed();
            let t2 = Instant::now();
            // Seed `lints` with the directive parser's own diagnostics
            // (`unknown-suppression-rule`, `empty-suppression`, …) and
            // the pragma walker's validation diagnostics (P40.3:
            // `conflicting-lint-pragma`, plus mirrored
            // `unknown-suppression-rule` / `empty-suppression` for the
            // `@lint_off` / `@lint_on` annotation form) so all of them
            // ride alongside regular lints into LSP / CLI surfaces.
            let mut lints = std::mem::take(&mut directives.diagnostics);
            lints.append(&mut pragmas.diagnostics);
            lints.extend(run_lints_with_directives(
                &hir,
                &resolutions,
                &index.symbols,
                &mut directives,
                bypass,
            ));
            let lint_took = t2.elapsed();
            PassAOut {
                uri,
                hir,
                lib,
                lower_took,
                directives,
                resolutions,
                lints,
                resolve_took,
                lint_took,
            }
        };

        let pass_a: Vec<PassAOut> = crate::parallel::par_map(hirs, pass_a_run);

        // Pass B (serial): body walker mutates `self.arena`.
        for p in pass_a {
            let mut timings = ModuleTimings {
                lower: p.lower_took,
                resolve: p.resolve_took,
                lint: p.lint_took,
                ..ModuleTimings::default()
            };
            let t1 = Instant::now();
            let mut analysis = analyze_with_index_into(
                &p.hir,
                &p.resolutions,
                &self.index,
                &self.decl_registry,
                &p.uri,
                &mut self.arena,
            );
            // Hard error pass: every `AnnotationArg::Invalid` left
            // by HIR lowering becomes a `Severity::Error`
            // structural diagnostic the package gate refuses on.
            // See `crate::annotation_validate` for the rationale.
            crate::annotation_validate::validate_annotation_args(
                &p.hir,
                &self.index,
                &mut analysis.diagnostics,
            );
            // Language-pragma contract validation (`@permission`, …).
            // Runs here so it sees the fully-built project index
            // (declared-permission set spans std + every included
            // module).
            crate::pragmas::validate_pragmas(&p.hir, &self.index, &mut analysis.diagnostics);
            timings.analyze = t1.elapsed();
            self.modules.insert(
                p.uri,
                ModuleAnalysis {
                    hir: p.hir,
                    resolutions: p.resolutions,
                    analysis,
                    lints: p.lints,
                    lib: p.lib,
                    timings,
                    directives: p.directives,
                },
            );
        }
    }

    /// Pass 3.7 — unified type-relation validation. Walks every
    /// module's HIR fresh AFTER all cross-module typing fixups have
    /// settled, re-runs each `is_assignable_to` check using the
    /// final `expr_types` / `def_types`, and emits diagnostics.
    ///
    /// Architectural invariant: NO type-relation diagnostic should
    /// be emitted earlier in the pipeline. The analyzer's
    /// per-module pass populates types but doesn't compare them;
    /// every `must be \`T\`, got \`U\`` flavor of error fires from
    /// this pass and only this pass. Otherwise the first-pass would
    /// see un-settled `any`s for cross-module Calls and surface
    /// false positives — the rubber-banding we kept hitting.
    ///
    /// Covers:
    ///
    /// - `if` / `while` / `do-while` / `for`-mid bool conditions
    /// - `var x: T = init` (top-level + local + type-attr inits)
    /// - `target = value` assignments
    /// - `return value;` value vs declared return type
    /// - call-site arg vs declared param type
    ///
    /// Modes:
    /// - `restrict = None` revalidates every cached module (used by
    ///   `rebuild`).
    /// - `restrict = Some(set)` only revalidates the listed URIs
    ///   the changed URI plus any module whose `expr_types` were
    ///   touched by the cross-module fixup passes. Used by
    ///   `invalidate` to keep per-keystroke cost bounded.
    ///
    // P16.6
    /// Typed lints that depend on settled per-expr types and
    /// the project-wide [`ProjectIndex`]. Runs after the cross-module
    /// type fixup passes (3.4 / 3.5) and before
    /// [`Self::validate_type_relations`]. Idempotent: drops prior
    /// findings for the rule before re-emitting.
    ///
    /// `restrict = None` lints every cached module; `Some(set)` only
    /// the listed URIs (matches the type-relation validation scope).
    fn run_typed_lints(&mut self, manager: &SourceManager, restrict: Option<&FxHashSet<&str>>) {
        let in_scope = |uri: &Uri| -> bool {
            match restrict {
                None => true,
                Some(set) => set.contains(uri.as_str()),
            }
        };
        // P19 — split borrows: read-only `&self.arena` + `&self.index`
        // alongside `&mut self.modules`.
        let arena = &self.arena;
        let index = &self.index;
        let decl_registry = &self.decl_registry;
        let bypass = self.bypass_suppressions;
        let enabled_rules = &self.enabled_rules;

        // P26.3 / P27.1 — every typed-lint pass takes `&arena`
        // (immutable) + `&index` (immutable) and writes only to its
        // module's own `lints` / `directives`. Different modules touch
        // disjoint memory, so the loop is embarrassingly parallel.
        //
        // Pre-extract each in-scope doc's `(text, tree)` into a
        // Send-safe map (Document is `!Sync` because of its
        // Parser + OnceCell, so we can't hold a `Ref<'_, _>` across
        // workers). Then collect `(uri, &mut ModuleAnalysis)` into a
        // Vec and dispatch through the `parallel::par_for_each` shim —
        // rayon on native, serial loop on wasm.
        #[allow(clippy::mutable_key_type)]
        let doc_data: FxHashMap<Uri, (String, Tree)> = self
            .modules
            .keys()
            .filter(|uri| in_scope(uri))
            .filter_map(|uri| {
                manager.get(uri).map(|cell| {
                    let doc = cell.borrow();
                    (uri.clone(), (doc.text.clone(), doc.tree.clone()))
                })
            })
            .collect();
        let modules: Vec<(&Uri, &mut ModuleAnalysis)> = self
            .modules
            .iter_mut()
            .filter(|(uri, _)| in_scope(uri))
            .collect();
        crate::parallel::par_for_each(modules, |(uri, module)| {
            run_typed_lints_for_module(
                uri,
                module,
                arena,
                index,
                decl_registry,
                bypass,
                enabled_rules,
                &doc_data,
            );
        });
    }

    fn validate_type_relations(&mut self, restrict: Option<&FxHashSet<&str>>) {
        use crate::analyzer::{DiagCategory, SemanticDiagnostic};

        let in_scope = |uri: &Uri| -> bool {
            match restrict {
                None => true,
                Some(set) => set.contains(uri.as_str()),
            }
        };

        // Idempotent: drop this pass's previous output for the URIs
        // we're about to revalidate. Modules outside `restrict` keep
        // their last-validated diagnostics — that's the whole point
        // of the incremental flow.
        for (uri, m) in self.modules.iter_mut() {
            if !in_scope(uri) {
                continue;
            }
            m.analysis
                .diagnostics
                .retain(|d| d.category != DiagCategory::TypeRelation);
        }

        // Architectural invariant — no producer outside this pass
        // may emit type-relation diagnostics. After the per-URI
        // clear above, every remaining TypeRelation diagnostic in
        // the cache is either from a prior validation run on an
        // out-of-scope module (correct) or from a buggy pre-pass
        // emitter (assertion catches it for in-scope modules).
        #[cfg(debug_assertions)]
        self.assert_no_in_scope_type_relation_diags(restrict);

        #[allow(clippy::mutable_key_type)]
        let mut diag_updates: FxHashMap<Uri, Vec<SemanticDiagnostic>> = FxHashMap::default();
        // P19 — split borrows: pass the shared arena alongside read-only
        // module borrows.
        let arena_mut = &mut self.arena;
        let index = &self.index;
        let decl_registry = &self.decl_registry;
        for (cur_uri, cur_module) in &self.modules {
            if !in_scope(cur_uri) {
                continue;
            }
            let mut diags: Vec<SemanticDiagnostic> = Vec::new();
            validate_module_type_relations(
                cur_module,
                cur_uri,
                index,
                decl_registry,
                arena_mut,
                &mut diags,
            );
            // Call-arg validation needs cross-module access (foreign
            // fn signatures), so it lives on `&self` rather than the
            // free walker. Note: we hold `arena_mut` here, so call into
            // a helper that accepts `&self.modules` + `&self.index` +
            // `arena` instead of borrowing `&self`.
            collect_call_arg_diags_split(
                &self.modules,
                index,
                decl_registry,
                cur_uri,
                arena_mut,
                &mut diags,
            );
            collect_object_field_diags_split(
                &self.modules,
                index,
                decl_registry,
                cur_uri,
                arena_mut,
                &mut diags,
            );
            collect_instance_method_value_ref_diags(&self.modules, cur_uri, &mut diags);
            collect_static_type_args_diags(&self.modules, cur_uri, &mut diags);
            collect_generic_arity_diags(cur_module, index, &mut diags);
            collect_object_construction_diags(
                &self.modules,
                arena_mut,
                index,
                decl_registry,
                cur_uri,
                &mut diags,
            );
            if !diags.is_empty() {
                diag_updates.insert(cur_uri.clone(), diags);
            }
        }
        for (uri, diags) in diag_updates {
            if let Some(m) = self.modules.get_mut(&uri) {
                m.analysis.diagnostics.extend(diags);
            }
        }
    }

    #[cfg(debug_assertions)]
    fn assert_no_in_scope_type_relation_diags(&self, restrict: Option<&FxHashSet<&str>>) {
        use crate::analyzer::DiagCategory;
        for (uri, m) in &self.modules {
            let in_scope = match restrict {
                None => true,
                Some(set) => set.contains(uri.as_str()),
            };
            if !in_scope {
                continue;
            }
            for d in &m.analysis.diagnostics {
                debug_assert!(
                    d.category != DiagCategory::TypeRelation,
                    "type-relation diagnostic emitted before validate_type_relations \
                     in {uri:?}: {msg}. Producer must defer to the validation post-pass — \
                     see DiagCategory.",
                    uri = uri.as_str(),
                    msg = d.message,
                );
            }
        }
    }

    // P14.9
    /// Walk every module's CST for qualified-name access
    /// patterns (`<module>::<name>`, `<module>::<type>::<name>`, etc.)
    /// and bump `references_to` for the matching decl in the named
    /// module. This is what lets the `unused-decl` lint correctly
    /// skip `private` decls that are only reachable through their
    /// fully-qualified name from other modules.
    ///
    /// Walks the **CST** rather than the HIR because nested
    /// `static_expr` shapes (`A::B::C`) don't lower cleanly into the
    /// current `StaticExpr { ty: TypeRef, property: Ident }` shape
    /// (the inner `A::B` would have to live in a `TypeRef` slot,
    /// which the grammar doesn't allow). The CST keeps the chain as
    /// nested `static_expr` nodes regardless.
    fn compute_qualified_refs(&mut self, manager: &SourceManager) {
        use greycat_analyzer_core::lsp_types::Uri as _Uri;

        // 1. module name → declaring URI.
        #[allow(clippy::mutable_key_type)]
        let mut by_name: FxHashMap<String, _Uri> = FxHashMap::default();
        for (uri, cell) in manager.iter() {
            let doc = cell.borrow();
            by_name.insert(doc.name().to_string(), uri.clone());
        }

        // 2. Walk every module's CST for `static_expr` nodes whose
        // chain root names a known module. Collect bumps.
        #[allow(clippy::mutable_key_type)]
        let mut bumps: FxHashMap<_Uri, Vec<Idx<Decl>>> = FxHashMap::default();
        for (uri, cell) in manager.iter() {
            let doc = cell.borrow();
            let text = &doc.text;
            let root = doc.root_node();
            greycat_analyzer_syntax::cst::walk_named(root, |node| {
                if node.kind() != "static_expr" {
                    return true;
                }
                // Outer static — only process top-level chains; inner
                // ones propagate from there. Skip if our parent is
                // also a `static_expr` (we'd double-count otherwise).
                if let Some(parent) = node.parent()
                    && parent.kind() == "static_expr"
                {
                    return true;
                }
                let chain = qualified_chain(node, text);
                if chain.len() < 2 {
                    return true;
                }
                let Some(target_uri) = by_name.get(&chain[0]) else {
                    return true;
                };
                // Skip self-references — qualified access to a decl
                // in the *current* module is treated as intra-module.
                if target_uri == uri {
                    return true;
                }
                let Some(target_module) = self.modules.get(target_uri) else {
                    return true;
                };
                // Match each subsequent ident in the chain against any
                // decl with that name in the target module. `chain[1]`
                // is the most common case (top-level decl); deeper
                // segments name attrs / methods / variants and are
                // outside the unused-decl lint's scope (intra-type
                // members aren't in `references_to`).
                let target_root = match target_module.hir.module.as_ref() {
                    Some(m) => m,
                    None => return true,
                };
                let needle = &chain[1];
                for decl_id in &target_root.decls {
                    if let Some(name_idx) = target_module.hir.decls[*decl_id].name()
                        && &self.index.symbols[target_module.hir.idents[name_idx].symbol] == needle
                    {
                        bumps.entry(target_uri.clone()).or_default().push(*decl_id);
                        break;
                    }
                }
                true
            });
        }

        // 3. Apply bumps.
        for (uri, decls) in bumps {
            let Some(m) = self.modules.get_mut(&uri) else {
                continue;
            };
            for decl in decls {
                *m.resolutions.references_to.entry(decl).or_insert(0) += 1;
            }
        }

        // 4. Re-run UnusedDecl with the enriched reference counts.
        // Other lints aren't affected by qualified refs, so we only
        // refresh that one rule per module.
        for module in self.modules.values_mut() {
            module.lints.retain(|l| l.rule != "unused-decl");
            let mut new_lints = Vec::new();
            let mut cx = crate::lint::LintCx::new(
                &module.hir,
                &module.resolutions,
                &self.index.symbols,
                Some(&mut module.directives),
                false,
                &mut new_lints,
            );
            lint::LintRule::check(&lint::UnusedDecl, &mut cx);
            module.lints.append(&mut new_lints);
        }
    }

    /// File-level invalidation: re-derive the analysis for `uri` only.
    /// The shared [`ProjectIndex`] is rebuilt over `manager`, reusing
    /// cached HIRs for documents that haven't changed so we don't
    /// re-lower the entire workspace on every keystroke.
    ///
    /// Drops cache entries for documents that are no longer in
    /// `manager` (e.g. closed without a follow-up `did_open`).
    pub fn invalidate(&mut self, manager: &SourceManager, uri: &Uri) {
        // Pragmatic incremental invalidation. The full Q1-Q5
        // query-DAG (separate hashes per stage, per-Uri Q5 cascade
        // filtering) is captured as a follow-up; today's
        // implementation is the minimum that's *correct* under P19-P23:
        //
        //   1. Drop dead URIs from the cache.
        //   2. Re-lower the changed doc; keep the rest of the HIRs.
        //   3. Rebuild the shared `ProjectIndex` from every module's
        //      HIR (so name tables / structure indices reflect the
        //      edit).
        //   4. **Re-run S7-S11 (`stage_lower_signatures`) project-wide.**
        //      Without this the post-P22 signature index goes stale
        //      whenever an attr / method / fn return type changes,
        //      and the analyzer types every dependent expr as `any`.
        //   5. Re-resolve + re-analyze only the changed module.
        //   6. Re-lint + re-validate the changed URI only (incremental).
        //
        // The expensive piece is step 4 — it walks every type / fn in
        // the project, but each TypeRef lowering is O(1) interned mints.
        // For a 50-file synthetic project that's still well under the
        // 50ms p99 target we're aiming for; tighter bounds (signature-
        // hash → skip step 4; cross-module-reference filter → skip
        // step 6 for unrelated URIs) move into the proper Q1-Q5 DAG.

        let live: FxHashSet<String> = manager
            .iter()
            .map(|(u, _)| u.as_str().to_string())
            .collect();
        self.modules.retain(|u, _| live.contains(u.as_str()));

        let mut lower_took = Duration::ZERO;
        let mut changed_lib: Option<String> = None;
        let mut changed_directives: Option<Directives> = None;
        let mut changed_pragmas: Option<LintPragmas> = None;
        let changed_hir = manager.get(uri).map(|cell| {
            let doc = cell.borrow();
            let start = Instant::now();
            // P35.1 — module name from URI for the module identity
            // that `ingest` keys decls by.
            let module_name = module_name_from_uri(uri).unwrap_or("module");
            let hir = lower_module(
                &doc.text,
                &self.index.symbols,
                module_name,
                &doc.lib,
                doc.root_node(),
            );
            lower_took = start.elapsed();
            changed_lib = Some(doc.lib.clone());
            changed_directives = Some(parse_directives(&doc.text, doc.root_node()));
            // P40.1 + P40.5 — re-parse the module's `@lint_off` /
            // `@lint_on` pragmas on every invalidate. Pass `is_entrypoint`
            // so the walker emits `lint-pragma-outside-entrypoint` when
            // pragmas show up in non-entrypoint modules.
            let is_entry = manager.entrypoint_uri() == Some(uri);
            changed_pragmas = Some(parse_lint_pragmas(&doc.text, doc.root_node(), is_entry));
            hir
        });

        // Rebuild `ProjectIndex` from scratch — `ingest` is additive
        // (no removal), so starting empty is what makes the changed
        // doc's deletions visible. **P19.9** — preserve the
        // [`SymbolTable`] so previously-issued [`Symbol`]s (e.g.
        // inside `sig_cache`) remain valid; only the per-module
        // index data gets wiped.
        let preserved_symbols = std::mem::take(&mut self.index.symbols);
        let mut new_index = ProjectIndex::new(preserved_symbols, &self.arena);
        if let Some(hir) = &changed_hir {
            new_index.ingest(uri, hir, &mut self.arena, &mut self.decl_registry);
        }
        for (other_uri, ma) in &self.modules {
            if other_uri == uri {
                continue;
            }
            new_index.ingest(other_uri, &ma.hir, &mut self.arena, &mut self.decl_registry);
        }
        // For docs that are in the manager but not yet in the cache,
        // lower them so the index sees their decls. Per-module analysis
        // runs only on their own invalidate call.
        let mut other_lowered: Vec<(Uri, Hir, String, Duration)> = Vec::new();
        for (other_uri, cell) in manager.iter() {
            if other_uri == uri || self.modules.contains_key(other_uri) {
                continue;
            }
            let doc = cell.borrow();
            // P35.1 — module name from URI for the module identity
            // that `ingest` keys decls by.
            let module_name = module_name_from_uri(other_uri).unwrap_or("module");
            let hir = lower_module(
                &doc.text,
                &self.index.symbols,
                module_name,
                &doc.lib,
                doc.root_node(),
            );
            new_index.ingest(other_uri, &hir, &mut self.arena, &mut self.decl_registry);
            other_lowered.push((other_uri.clone(), hir, doc.lib.clone(), Duration::ZERO));
        }
        self.index = new_index;

        let Some(hir) = changed_hir else {
            self.modules.remove(uri);
            return;
        };

        // Feed every cached + freshly-lowered HIR through
        // `lower_signatures_into` so `index.type_members
        // .{attr_types, method_returns}` / `index.fn_signatures` /
        // `index.enum_types` reflect the post-edit signatures. The
        // free-function variant takes split `&mut TypeArena` and
        // `&mut ProjectIndex` borrows so we can build the slice from
        // references into `self.modules` — no `Hir` clone.
        {
            let mut pairs: Vec<(&Uri, &Hir)> = Vec::with_capacity(self.modules.len() + 1);
            pairs.push((uri, &hir));
            for (other_uri, other_hir, _, _) in &other_lowered {
                pairs.push((other_uri, other_hir));
            }
            for (other_uri, ma) in &self.modules {
                if other_uri == uri {
                    continue;
                }
                pairs.push((other_uri, &ma.hir));
            }
            lower_signatures_into(
                &mut self.arena,
                &mut self.index,
                &self.decl_registry,
                &pairs,
                &mut self.sig_cache,
            );
        }

        let mut timings = ModuleTimings {
            lower: lower_took,
            ..ModuleTimings::default()
        };
        let t0 = Instant::now();
        let resolutions =
            self.index
                .resolutions(&hir, Some(uri), Some(self.arena.builtins.map_key));
        timings.resolve = t0.elapsed();
        let t1 = Instant::now();
        let mut analysis = analyze_with_index_into(
            &hir,
            &resolutions,
            &self.index,
            &self.decl_registry,
            uri,
            &mut self.arena,
        );
        // Hard-error pragma passes — must run on the incremental path
        // too, not just the full analyze, or `invalid-pragma-arg` /
        // `unknown-permission` silently stop re-emitting after an edit.
        crate::annotation_validate::validate_annotation_args(
            &hir,
            &self.index,
            &mut analysis.diagnostics,
        );
        crate::pragmas::validate_pragmas(&hir, &self.index, &mut analysis.diagnostics);
        timings.analyze = t1.elapsed();
        let t2 = Instant::now();
        let mut directives = changed_directives.unwrap_or_default();
        let bypass = self.bypass_suppressions;
        let mut lints = std::mem::take(&mut directives.diagnostics);
        // P40.3 + P40.5 — seed pragma-walker diagnostics
        // (`unknown-suppression-rule`, `empty-suppression`,
        // `conflicting-lint-pragma` for the entrypoint;
        // `lint-pragma-outside-entrypoint` for other modules) into
        // the lints stream just like `stage_per_module_analysis` does.
        if let Some(pragmas) = changed_pragmas.as_mut() {
            lints.append(&mut pragmas.diagnostics);
        }
        lints.extend(run_lints_with_directives(
            &hir,
            &resolutions,
            &self.index.symbols,
            &mut directives,
            bypass,
        ));
        timings.lint = t2.elapsed();
        self.modules.insert(
            uri.clone(),
            ModuleAnalysis {
                hir,
                resolutions,
                analysis,
                lints,
                lib: changed_lib.unwrap_or_else(|| "project".to_string()),
                timings,
                directives,
            },
        );
        // P40.1 — re-fold the entrypoint's pragmas if THIS invalidation
        // hit the entrypoint, so a project-wide policy edit flows in
        // immediately. The LSP path (the only `invalidate` caller)
        // doesn't populate `enabled_rules` / `disabled_rules` from any
        // other source, so wholesale replacement is sound. When LSP
        // config eventually feeds these sets, a `Project::cli_enabled`
        // sibling field will let the union be recomputed without
        // losing external state — but that's not on the P40.1 path.
        if manager.entrypoint_uri() == Some(uri)
            && let Some(pragmas) = changed_pragmas
        {
            self.disabled_rules = pragmas.off;
            self.enabled_rules = pragmas.on;
        }
        // P22-P23 — passes 3.4 / 3.45 / 3.5 / 3.52 are gone; cross-
        // module typing happens inline in the analyzer's body walker.
        // Only the typed-lint pass and type-relation validation remain
        // — both run on the changed URI only here for incremental cost.
        let mut touched: FxHashSet<&str> = FxHashSet::default();
        touched.insert(uri.as_str());
        self.run_typed_lints(manager, Some(&touched));
        self.validate_type_relations(Some(&touched));
        self.compute_qualified_refs(manager);
        // P40.1 — same rule-policy sweep as `analyze_staged` does at
        // the tail. Restricted to `touched` so the incremental cost
        // stays per-edit.
        self.apply_rule_policy(Some(&touched));
    }

    pub fn module(&self, uri: &Uri) -> Option<&ModuleAnalysis> {
        self.modules.get(uri)
    }

    // P19.6
    /// Number of modules currently held in the
    /// signature-stage cache. Exposed for the test that asserts the
    /// cache is populated after a build / partially refreshed after
    /// a body-only edit. Production callers shouldn't depend on the
    /// exact value.
    #[doc(hidden)]
    pub fn sig_cache_len(&self) -> usize {
        self.sig_cache.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Uri, &ModuleAnalysis)> {
        self.modules.iter()
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

// P14.9
/// Pull every ident text from a `static_expr` chain (left to
/// right). For `runtime::ResponseCode::ok` returns
/// `["runtime", "ResponseCode", "ok"]`. The leftmost segment comes
/// from the chain root's `type_ident.name`; subsequent segments come
/// from each enclosing `static_expr.property`.
fn qualified_chain(node: Node<'_>, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect_chain(node, text, &mut out);
    out
}

fn collect_chain(node: Node<'_>, text: &str, out: &mut Vec<String>) {
    if node.kind() == "static_expr" {
        // Recurse into the left side first, then append our property.
        let property = node.child_by_field_name("property");
        let left = node
            .named_children(&mut node.walk())
            .find(|c| Some(c.id()) != property.map(|p| p.id()));
        if let Some(left) = left {
            collect_chain(left, text, out);
        }
        if let Some(p) = property
            && let Some(s) = text.get(p.byte_range())
        {
            out.push(s.to_string());
        }
    } else if node.kind() == "type_ident"
        && let Some(name) = node.child_by_field_name("name")
        && let Some(s) = text.get(name.byte_range())
    {
        out.push(s.to_string());
    }
}

// P15.10
/// Resolve a call's callee to its declaring `Decl::Fn`.
/// Returns `(Some(foreign_uri), decl_id)` for cross-module callees
/// and `(None, decl_id)` for in-module callees.
///
/// Covers these callee shapes:
///   * `Expr::Ident` -> `Definition::Decl(Decl::Fn)` (in-module top-level).
///   * `Expr::Ident` -> `Definition::ProjectDecl { uri, decl }` where decl is `Decl::Fn`.
///   * `Expr::Static` -> `member_uses` -> `MemberDef::Method(decl_id)` (intra-module).
///   * `Expr::Static` -> `foreign_member_uses` -> `MemberDef::Method(decl_id)` (cross-module).
///   * `Expr::Member` / `Expr::Arrow` -> same `member_uses` / `foreign_member_uses` path
///     (the analyzer's `resolve_member` populates the same maps for `f.method(...)` and
///     `n->method(...)`), so method calls go through the same arity / arg-type checks as
///     bare-fn / static calls.
///   * `Expr::QualifiedStatic` -> `resolve_qualified_chain` -> `MemberDef::Method`.
///
/// Lambda callees and unresolved member accesses return `None` from
/// `resolve_call_target` — they take the lambda-callee fallback path in
/// [`lambda_call_arg_diags`] below.
//
/// Lambda-callee arm of the call-arg validator. Reads the callee's
/// settled `expr_types` entry; if it's `TypeKind::Lambda { params, ret }`
/// (a lambda literal stored in a var, or a fn-ref minted by
/// `Analyzer::fn_ref_ty_from_sig` in step 2 of the lambda-unify plan),
/// emits the same `call-arity` and `argument-type-mismatch` diagnostics
/// as the fn-decl path against the lambda's pre-lowered params.
///
/// Opaque `function` (the nominal `Type(function_decl)`) carries no
/// signature and is skipped — the call still types as `any?` via the
/// `Expr::Call` default, no validation possible. Same for any other
/// `TypeKind` (unresolved, primitive, etc.).
///
/// Strips the outer nullable bit before inspection so `function?`-typed
/// callees still validate against the underlying lambda shape, mirroring
/// the runtime which null-checks once then dispatches against the
/// signature.
#[allow(clippy::too_many_arguments)]
fn lambda_call_arg_diags(
    cur_module: &ModuleAnalysis,
    cur_uri: &Uri,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    call: &greycat_analyzer_hir::hir::CallExpr,
    diags: &mut Vec<crate::analyzer::SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_core::TypeKind;

    let Some(callee_ty) = cur_module.analysis.expr_types.get(&call.callee).copied() else {
        return;
    };
    // Clone the kind so we can read params / ret without holding the
    // arena borrow across the diagnostic loop (which may allocate via
    // `display_type` -> nullable wrappers internally).
    let kind = arena.get(callee_ty).kind.clone();
    let (params, _ret) = match kind {
        TypeKind::Lambda { params, ret } => (params, ret),
        _ => return,
    };
    let expected = params.len();
    let actual = call.args.len();
    if expected != actual {
        let callee_end = cur_module.hir.exprs[call.callee].byte_range().end;
        let plural = if expected == 1 { "" } else { "s" };
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "call-arity",
            message: format!("call expects {expected} argument{plural}, but got {actual}"),
            byte_range: callee_end..call.byte_range.end,
            category: DiagCategory::TypeRelation,
        });
        return;
    }
    for i in 0..expected {
        let declared_ty = params[i];
        let arg_ty = match cur_module.analysis.expr_types.get(&call.args[i]).copied() {
            Some(t) => t,
            None => continue,
        };
        if !is_assignable_to_with_index(index, decl_registry, arena, arg_ty, declared_ty) {
            let r = cur_module.hir.exprs[call.args[i]].byte_range();
            diags.push(SemanticDiagnostic {
                severity: Severity::Error,
                code: "argument-type-mismatch",
                message: format!(
                    "value of type `{}` is not assignable to parameter of type `{}`",
                    display_type_for_module(arena, index, decl_registry, arg_ty, Some(cur_uri),),
                    display_type_for_module(
                        arena,
                        index,
                        decl_registry,
                        declared_ty,
                        Some(cur_uri),
                    ),
                ),
                byte_range: r,
                category: DiagCategory::TypeRelation,
            });
        }
    }
}

/// Walks every module's `Expr::Call` and emits arity / arg-type
/// diagnostics. Two signature carriers are supported:
///
/// 1. Direct fn-decl callees (in-module via `Resolutions::uses` +
///    `member_uses`, cross-module via `foreign_member_uses` +
///    `QualifiedStatic`) — resolved via [`resolve_call_target`].
/// 2. Lambda-typed callees — when (1) returns `None`, the callee's
///    settled `expr_types` entry is consulted; if it's
///    `TypeKind::Lambda { params, ret }` (a lambda literal in a
///    var, or a fn-ref minted by `fn_ref_ty_from_sig`), arity and
///    per-arg checks run against the lambda's pre-lowered params.
///    Opaque `function` and everything else carry no signature
///    and are skipped here.
///
/// Runs after pass 3.5 so the arg-side `expr_types` reflect any
/// cross-module return-type inferences (otherwise outer calls
/// whose args are inner static-expr calls would all surface
/// "value of type `any`" false positives). Folded into the
/// unified validation phase so all type-relation diagnostics share
/// one producer.
// P19
/// Split-borrow variant: takes `&modules`, `&index`, and a
/// mutable borrow on the shared arena. The validation loop holds
/// `&mut self.arena` during iteration over `&self.modules`, so the
/// `&self`-borrowing version can no longer be invoked directly
/// from the same scope.
/// Emit the `generic-erasure` diagnostic: a value whose analyzer-
/// materialized type is assignable to `slot_desc`, but whose *runtime*
/// type — after the GreyCat runtime erases the producing fn's generic to
/// `any?` (see [`crate::erasure`]) — is not. Verified against
/// `greycat run`: the runtime throws `… not assignable …` at exactly
/// these sites (arg-passing, field init, return). Error severity, no
/// suppression — it's a real runtime crash the analyzer would otherwise
/// hide behind its optimistic monomorphization.
#[allow(clippy::too_many_arguments)]
fn push_generic_erasure_diag(
    diags: &mut Vec<SemanticDiagnostic>,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    cur_uri: &Uri,
    runtime_ty: TypeId,
    slot_desc: String,
    byte_range: std::ops::Range<usize>,
) {
    use crate::analyzer::{DiagCategory, Severity};
    let runtime_disp =
        display_type_for_module(arena, index, decl_registry, runtime_ty, Some(cur_uri));
    diags.push(SemanticDiagnostic {
        severity: Severity::Error,
        code: "generic-erasure",
        message: format!(
            "this value is `{runtime_disp}` at runtime — GreyCat erases \
             function-generic type parameters to `any?` — which is not assignable \
             to {slot_desc}, so it throws at runtime"
        ),
        byte_range,
        category: DiagCategory::TypeRelation,
    });
}

/// Assignability for a construction *slot* — positional element, `geo`
/// component, map key / value: [`is_assignable_to_with_index`] plus the
/// `int → float` widening the runtime applies in container / binding
/// positions (`node<float> { 3 }` ≡ `{ 3.0 }`, `geo { 1, 2 }` ≡
/// `{ 1.0, 2.0 }`). `float → int` and every non-numeric mismatch stay
/// rejected. Named-field slots deliberately use the strict relation
/// directly — the runtime rejects `T { f: 1 }` where named fields don't
/// coerce.
fn is_slot_assignable(
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    from: TypeId,
    to: TypeId,
) -> bool {
    if is_assignable_to_with_index(index, decl_registry, arena, from, to) {
        return true;
    }
    let from_t = arena.get(from);
    let to_t = arena.get(to);
    // A nullable source can't flow into a non-nullable slot.
    if from_t.nullable && !to_t.nullable {
        return false;
    }
    from == arena.builtins.int && to == arena.builtins.float
}

/// Check one supplied construction value against an expected slot type,
/// mirroring the two-tier check the named-field / call-arg validators
/// run: a hard `code` mismatch when the value type isn't assignable,
/// else a `generic-erasure` diag when the materialized type fits but its
/// runtime-erased shape doesn't. `slot_desc` is the noun phrase naming
/// the slot (`` element type `int` ``, `` map key type `String` ``).
/// Slot assignability is [`is_slot_assignable`] — `int → float` widens.
#[allow(clippy::too_many_arguments)]
fn check_construction_value_against_slot(
    cur_module: &ModuleAnalysis,
    cur_uri: &Uri,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    value_expr: Idx<Expr>,
    expected_ty: TypeId,
    code: &'static str,
    slot_desc: &str,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::Severity;

    let Some(value_ty) = cur_module.analysis.expr_types.get(&value_expr).copied() else {
        return;
    };
    if !is_slot_assignable(index, decl_registry, arena, value_ty, expected_ty) {
        let r = cur_module.hir.exprs[value_expr].byte_range();
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code,
            message: format!(
                "value of type `{}` is not assignable to {slot_desc}",
                display_type_for_module(arena, index, decl_registry, value_ty, Some(cur_uri)),
            ),
            byte_range: r,
            category: DiagCategory::TypeRelation,
        });
    } else if let Some(runtime_ty) = cur_module
        .analysis
        .expr_runtime_types
        .get(&value_expr)
        .copied()
        && !is_slot_assignable(index, decl_registry, arena, runtime_ty, expected_ty)
    {
        let r = cur_module.hir.exprs[value_expr].byte_range();
        push_generic_erasure_diag(
            diags,
            arena,
            index,
            decl_registry,
            cur_uri,
            runtime_ty,
            slot_desc.to_string(),
            r,
        );
    }
}

#[allow(clippy::mutable_key_type)]
fn collect_call_arg_diags_split(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    cur_uri: &Uri,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::hir::Expr;

    let cur_module = match modules.get(cur_uri) {
        Some(m) => m,
        None => {
            return;
        }
    };
    for (_call_id, call_expr) in cur_module.hir.exprs.iter() {
        let Expr::Call(call) = call_expr else {
            continue;
        };
        let Some((foreign_uri_opt, fn_decl_id)) =
            resolve_call_target(modules, index, cur_module, call.callee)
        else {
            // No fn-decl handle — fall back to the callee's settled
            // type. Lambda-typed callees (lambda literals / fn-ref
            // values) carry their signature in the type; opaque
            // `function` and other kinds skip.
            lambda_call_arg_diags(
                cur_module,
                cur_uri,
                index,
                decl_registry,
                arena,
                call,
                diags,
            );
            continue;
        };
        let foreign_module = match &foreign_uri_opt {
            Some(u) => modules.get(u),
            None => Some(cur_module),
        };
        let Some(fn_module) = foreign_module else {
            continue;
        };
        let Decl::Fn(fnd) = &fn_module.hir.decls[fn_decl_id] else {
            continue;
        };
        // **P19.19** — arity check (independent of generics; a
        // generic fn `<T>(x: T)` still has arity 1). Mirrors the TS
        // reference's "Function 'foo' expects N arguments, but got
        // M" diagnostic. Highlight range = the arg-list parens
        // (callee_end..call_end), matching the reference's span.
        let expected = fnd.params.len();
        let actual = call.args.len();
        if expected != actual {
            let fn_name = &index.symbols[fn_module.hir.idents[fnd.name].symbol];
            let callee_end = cur_module.hir.exprs[call.callee].byte_range().end;
            let plural = if expected == 1 { "" } else { "s" };
            diags.push(SemanticDiagnostic {
                severity: Severity::Error,
                code: "call-arity",
                message: format!(
                    "function `{fn_name}` expects {expected} argument{plural}, but got {actual}"
                ),
                byte_range: callee_end..call.byte_range.end,
                category: DiagCategory::TypeRelation,
            });
            // Skip the per-arg type validation: the pair-mapping is
            // ambiguous when arity is wrong, so further diagnostics
            // would be noise.
            continue;
        }
        if !fnd.generics.is_empty() {
            continue;
        }
        // Method call on a generic receiver: substitute the
        // enclosing type's generic params into each declared
        // method-param type. For `n.set(42)` with `n: node<int?>`,
        // this turns `set(value: T)` into `set(value: int?)` before
        // the arg-type comparison so we don't surface a false
        // "value of type `int` is not assignable to parameter
        // `value: T`" diagnostic.
        // Returns two pieces:
        //   - `generics_in_scope` so `lower_type_ref_project`
        //     mints the method's `T` as `GenericParam{T, owner=Type(node)}`
        //     (rather than the default `Unresolved{T}` which would
        //     accept anything).
        //   - `subst` mapping `{T → int?}` so `arena.substitute`
        //     replaces the `GenericParam` with the concrete arg.
        // Both maps are empty for non-member callees / non-generic
        // receivers; the lowering then collapses to the
        // empty-scope path and substitution is a no-op.
        let (method_generics_in_scope, method_subst) = method_subst_from_receiver(
            arena,
            cur_module,
            fn_module,
            index,
            &cur_module.hir.exprs[call.callee],
        );
        let pair_count = fnd.params.len().min(call.args.len());
        for i in 0..pair_count {
            let p = &fn_module.hir.fn_params[fnd.params[i]];
            let Some(declared_ref) = p.ty else {
                continue;
            };
            // Lower the method's param TypeRef directly into the
            // shared arena (`lower_type_ref_project` returns a
            // `TypeId`, no intermediate shape needed), then
            // substitute the receiver-derived generic args via
            // `arena.substitute`. Replaces the old TypeShape
            // round-trip — one less type representation, fewer
            // per-call allocations, no foreign-HIR re-walk.
            let fn_uri = foreign_uri_opt.as_ref().unwrap_or(cur_uri);
            let declared_raw = lower_type_ref_project(
                &fn_module.hir,
                declared_ref,
                arena,
                index,
                decl_registry,
                &method_generics_in_scope,
                Some(fn_uri),
            );
            let declared_ty = if method_subst.is_empty() {
                declared_raw
            } else {
                arena.substitute(declared_raw, &method_subst)
            };
            let arg_ty = match cur_module.analysis.expr_types.get(&call.args[i]).copied() {
                Some(t) => t,
                None => continue,
            };
            if !is_assignable_to_with_index(index, decl_registry, arena, arg_ty, declared_ty) {
                let p_name = &index.symbols[fn_module.hir.idents[p.name].symbol];
                let r = cur_module.hir.exprs[call.args[i]].byte_range();
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "argument-type-mismatch",
                    message: format!(
                        "value of type `{}` is not assignable to parameter `{}: {}`",
                        display_type_for_module(
                            arena,
                            index,
                            decl_registry,
                            arg_ty,
                            Some(cur_uri),
                        ),
                        p_name,
                        display_type_for_module(
                            arena,
                            index,
                            decl_registry,
                            declared_ty,
                            Some(cur_uri),
                        ),
                    ),
                    byte_range: r,
                    category: DiagCategory::TypeRelation,
                });
            } else if let Some(runtime_ty) = cur_module
                .analysis
                .expr_runtime_types
                .get(&call.args[i])
                .copied()
                && !is_assignable_to_with_index(
                    index,
                    decl_registry,
                    arena,
                    runtime_ty,
                    declared_ty,
                )
            {
                // The materialized arg type fits, but the runtime-erased
                // shape doesn't — the runtime throws here.
                let p_name = &index.symbols[fn_module.hir.idents[p.name].symbol];
                let declared_disp = display_type_for_module(
                    arena,
                    index,
                    decl_registry,
                    declared_ty,
                    Some(cur_uri),
                );
                let r = cur_module.hir.exprs[call.args[i]].byte_range();
                push_generic_erasure_diag(
                    diags,
                    arena,
                    index,
                    decl_registry,
                    cur_uri,
                    runtime_ty,
                    format!("parameter `{p_name}: {declared_disp}`"),
                    r,
                );
            }
        }
    }
}

/// Sibling of [`Self::collect_call_arg_diags_split`] for the
/// object-construction shape: `Foo<int> { quantizers: false }`.
/// Every supplied field's value type must be assignable to the
/// attr's declared type, after substituting the object expr's own
/// generic args for the decl's `GenericParam` slots. Walks the
/// supertype chain so inherited attrs (`type Dog extends Animal`,
/// `Dog { name: ... }`) check against `Animal`'s declared `name`
/// type. Statics are skipped — the analyzer's unknown-field check
/// handles `Foo { static_attr_name: ... }` separately.
///
/// Sibling of the structural `check_object_required_attrs` (in
/// the analyzer) which does name-level completeness. Type-relation
/// work lives here per the architectural invariant on
/// [`Self::validate_type_relations`].
#[allow(clippy::mutable_key_type)]
fn collect_object_field_diags_split(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    cur_uri: &Uri,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::hir::Expr;

    let cur_module = match modules.get(cur_uri) {
        Some(m) => m,
        None => {
            return;
        }
    };
    for (obj_expr_id, expr) in cur_module.hir.exprs.iter() {
        let Expr::Object(obj_expr) = expr else {
            continue;
        };
        let Some(obj_ty) = cur_module.analysis.expr_types.get(&obj_expr_id).copied() else {
            continue;
        };
        // `Map<K, V> { k: v }` is a named-form head whose entries are
        // key/value value-exprs, not attrs — `k` types against `K`, `v`
        // against `V`. Keyed on the settled `obj_ty` decl (not
        // `item_id_for`, which resolves `Map` to its seeded builtin id,
        // not the `type_members` key) and dispatched before the
        // attr-chain guards below, which Map has no entry for.
        let map_kv = match &arena.get(obj_ty).kind {
            TypeKind::Generic { tpl, args }
                if *tpl == arena.builtins.map_key && args.len() == 2 =>
            {
                Some((args[0], args[1]))
            }
            _ => None,
        };
        if let Some((key_ty, val_ty)) = map_kv {
            let key_desc = format!(
                "map key type `{}`",
                display_type_for_module(arena, index, decl_registry, key_ty, Some(cur_uri)),
            );
            let val_desc = format!(
                "map value type `{}`",
                display_type_for_module(arena, index, decl_registry, val_ty, Some(cur_uri)),
            );
            for f in obj_expr.fields.iter() {
                check_construction_value_against_slot(
                    cur_module,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    f.name,
                    key_ty,
                    "map-key-type-mismatch",
                    &key_desc,
                    diags,
                );
                check_construction_value_against_slot(
                    cur_module,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    f.value,
                    val_ty,
                    "map-value-type-mismatch",
                    &val_desc,
                    diags,
                );
            }
            continue;
        }
        // Attr-chain path (every named head except `Map`). Positional
        // construction is a separate HIR variant (`PositionalObject`)
        // and never reaches this named-only validator. Take the head
        // decl from the settled object type — provenance-blind, so
        // foreign / qualified heads check identically to same-module
        // ones (the old `item_id_for(cur_uri, …)` fabricated a same-
        // module ItemKey and silently skipped every cross-module head).
        let head_id = match &arena.get(obj_ty).kind {
            TypeKind::Generic { tpl, .. } => *tpl,
            TypeKind::Type(decl) => *decl,
            _ => continue,
        };
        let Some(head_members) = index.type_members.get(&head_id) else {
            continue;
        };
        // Build substitution from the object expr's own settled TypeId.
        // Non-generic head ⇒ empty subst (a no-op). Arity mismatch ⇒
        // skip the whole expr; the head's lowering pass already flagged
        // it elsewhere and substituting with the wrong shape would
        // surface noise.
        let init_subst: FxHashMap<Symbol, TypeId> = match &arena.get(obj_ty).kind {
            TypeKind::Generic { args, .. } if args.len() == head_members.generics.len() => {
                head_members
                    .generics
                    .iter()
                    .copied()
                    .zip(args.iter().copied())
                    .collect()
            }
            TypeKind::Type(_) if head_members.generics.is_empty() => FxHashMap::default(),
            _ => continue,
        };
        // Walk the chain Sub → Base<int> → Base<int>'s parent …,
        // accumulating each level's subst so an attr inherited from
        // a generic parent (`val: T` on `Base<T>`) gets `T`
        // substituted with the concrete arg the child instantiates
        // (`Sub extends Base<int>` → `val: int`). Mirrors the
        // [`walk_substituted_supertype_chain`] flow used by
        // assignability; we can't share that helper here because
        // we need each hop's attr table, not just the final
        // assignability result.
        //
        // `chain_attrs` stores the *already-substituted* declared
        // type per attr, so the per-field check is a direct lookup
        // — no second-pass substitution needed.
        let mut chain_attrs: FxHashMap<Symbol, (TypeId, bool)> = FxHashMap::default();
        let mut cur_tpl = head_id;
        let mut cur_subst = init_subst;
        let mut seen: FxHashSet<ItemKey> = FxHashSet::default();
        for _ in 0..32 {
            if !seen.insert(cur_tpl) {
                break;
            }
            let Some(m) = index.type_members.get(&cur_tpl) else {
                break;
            };
            for (sym, raw_ty) in &m.attr_types {
                let ty = if cur_subst.is_empty() {
                    *raw_ty
                } else {
                    arena.substitute(*raw_ty, &cur_subst)
                };
                let is_static = m.static_attrs.contains(sym);
                chain_attrs.entry(*sym).or_insert((ty, is_static));
            }
            let Some(sup_ty_raw) = m.supertype_ty else {
                break;
            };
            let sup_ty = if cur_subst.is_empty() {
                sup_ty_raw
            } else {
                arena.substitute(sup_ty_raw, &cur_subst)
            };
            match arena.get(sup_ty).kind.clone() {
                TypeKind::Generic { tpl, args } => {
                    let Some(parent_m) = index.type_members.get(&tpl) else {
                        break;
                    };
                    if parent_m.generics.len() != args.len() {
                        break;
                    }
                    cur_tpl = tpl;
                    cur_subst = parent_m
                        .generics
                        .iter()
                        .copied()
                        .zip(args.iter().copied())
                        .collect();
                }
                TypeKind::Type(decl) => {
                    cur_tpl = decl;
                    cur_subst.clear();
                }
                _ => break,
            }
        }
        for f in obj_expr.fields.iter() {
            // The key is a field name only when it's an ident / quoted
            // string; any other key shape (a `Map` value-key) has no
            // attr to type-check against here.
            let Some((attr_sym, _)) =
                crate::analyzer::object_field_key_name(&cur_module.hir, &index.symbols, f.name)
            else {
                continue;
            };
            let Some(&(declared_ty, is_static)) = chain_attrs.get(&attr_sym) else {
                continue; // unknown-field handled by the structural pass
            };
            if is_static {
                continue; // unknown-field also handles static-as-instance
            }
            let Some(value_ty) = cur_module.analysis.expr_types.get(&f.value).copied() else {
                continue;
            };
            if !is_assignable_to_with_index(index, decl_registry, arena, value_ty, declared_ty) {
                let attr_name = &index.symbols[attr_sym];
                let r = cur_module.hir.exprs[f.value].byte_range();
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "field-type-mismatch",
                    message: format!(
                        "value of type `{}` is not assignable to field `{}: {}`",
                        display_type(arena, decl_registry, &index.symbols, value_ty),
                        attr_name,
                        display_type(arena, decl_registry, &index.symbols, declared_ty),
                    ),
                    byte_range: r,
                    category: DiagCategory::TypeRelation,
                });
            } else if let Some(runtime_ty) = cur_module
                .analysis
                .expr_runtime_types
                .get(&f.value)
                .copied()
                && !is_assignable_to_with_index(
                    index,
                    decl_registry,
                    arena,
                    runtime_ty,
                    declared_ty,
                )
            {
                // Materialized value fits the field, but its runtime-
                // erased shape doesn't — constructing this object throws.
                let attr_name = &index.symbols[attr_sym];
                let declared_disp =
                    display_type(arena, decl_registry, &index.symbols, declared_ty).to_string();
                let r = cur_module.hir.exprs[f.value].byte_range();
                push_generic_erasure_diag(
                    diags,
                    arena,
                    index,
                    decl_registry,
                    cur_uri,
                    runtime_ty,
                    format!("field `{attr_name}: {declared_disp}`"),
                    r,
                );
            }
        }
    }
}

/// Instance-method value-reference walker (lambda-unify step 4).
///
/// Only context-free fn references are first-class values in GCL:
/// top-level fns and `static` methods. An instance method carries an
/// implicit `this`, which has no representation as a free function
/// value — taking `obj.m` / `Foo::m` (where `m` is non-static) and
/// using it as a value (not as the callee of a `Call`) is a hard
/// error.
///
/// Walks every `Expr::Member` / `Expr::Arrow` / `Expr::Static` whose
/// resolved member is a non-static method. The expression is in
/// "value position" iff it is NOT the callee of an enclosing
/// `Expr::Call` — a one-pass scan collects all callee `Idx<Expr>`s
/// into a set, then negation gives the value-position set.
///
/// Cross-module decls are read through `foreign_member_uses.uri` →
/// `modules[uri].hir.decls[decl_id].modifiers.static_`. No FnSignature
/// lookup needed; the modifier bit is on the HIR decl directly.
#[allow(clippy::mutable_key_type)]
fn collect_instance_method_value_ref_diags(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    cur_uri: &Uri,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, MemberDef, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::hir::Expr;
    use rustc_hash::FxHashSet;

    let Some(cur_module) = modules.get(cur_uri) else {
        return;
    };
    // Collect every callee Idx so we can ask "is this expr the callee
    // of a Call?" in O(1).
    let mut callee_set: FxHashSet<Idx<Expr>> = FxHashSet::default();
    for (_eid, expr) in cur_module.hir.exprs.iter() {
        if let Expr::Call(call) = expr {
            callee_set.insert(call.callee);
        }
    }

    for (expr_id, expr) in cur_module.hir.exprs.iter() {
        if callee_set.contains(&expr_id) {
            continue;
        }
        let (property, byte_range) = match expr {
            Expr::Member(m) | Expr::Arrow(m) => (m.property.ident(), m.byte_range.clone()),
            Expr::Static(s) => (s.property.ident(), s.byte_range.clone()),
            _ => continue,
        };
        // In-module binding.
        let (member, decl_uri) = if let Some(member) = cur_module.analysis.member_lookup(property) {
            (member, cur_uri.clone())
        } else if let Some(foreign) = cur_module.analysis.foreign_member_lookup(property) {
            (foreign.member, foreign.uri.clone())
        } else {
            continue;
        };
        let MemberDef::Method(decl_id) = member else {
            continue;
        };
        let decl_module = match modules.get(&decl_uri) {
            Some(m) => m,
            None => continue,
        };
        let Decl::Fn(fnd) = &decl_module.hir.decls[decl_id] else {
            continue;
        };
        if fnd.modifiers.static_ {
            continue;
        }
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "instance-method-value-ref",
            message: "cannot take a reference to an instance method \
                      (only top-level functions and `static` methods are first-class values)."
                .to_string(),
            byte_range,
            category: DiagCategory::TypeRelation,
        });
    }
}

/// Reject generic type arguments on a static access — `Foo<int>::bar()`,
/// `Foo<int>::bar` (value-ref), `Foo<int>::ATTR`.
///
/// GreyCat has no bounded generics, so the type parameter is inert in any
/// static context: a static carries no instance to bind `T` from, can't
/// construct one (`T {}` is rejected), and can't dispatch on it. The
/// `<...>` therefore never changes which code runs or what it returns —
/// in *any* program. The runtime rejects the construct outright (with an
/// unhelpful "syntax error"); we mirror it as a precise hard error whose
/// span is the removable `<...>` slice, so the auto-fix can strip it back
/// to `Foo::bar()`. See [`crate::ide::quickfix::edit_for_diagnostic`].
///
/// Tagged `TypeRelation` so the `validate_type_relations` retain-and-
/// re-emit cycle refreshes it on every incremental edit, matching
/// [`collect_instance_method_value_ref_diags`].
#[allow(clippy::mutable_key_type)]
fn collect_static_type_args_diags(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    cur_uri: &Uri,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::hir::Expr;

    let Some(cur_module) = modules.get(cur_uri) else {
        return;
    };
    for (_eid, expr) in cur_module.hir.exprs.iter() {
        let Expr::Static(s) = expr else {
            continue;
        };
        let ty = &cur_module.hir.type_refs[s.ty];
        if ty.params.is_empty() {
            continue;
        }
        // Span the `<...>` slice only — from the end of the type name
        // through the end of the type reference (the closing `>`). The
        // squiggle and the auto-fix both target the removable noise and
        // leave `Foo` intact.
        let name_end = cur_module.hir.idents[ty.name].byte_range.end;
        let close = ty.byte_range.end;
        if close <= name_end {
            continue;
        }
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "static-type-args",
            message: "generic type arguments are not allowed on a static access".to_string(),
            byte_range: name_end..close,
            category: DiagCategory::TypeRelation,
        });
    }
}

/// Flag a generic type reference instantiated with the wrong number of
/// type arguments — `nodeTime<int, float>` (nodeTime takes 1), `Map<int>`
/// (Map takes 2), `Box<int, float>` (a user `Box<T>` takes 1). The
/// runtime rejects these with "<T> defines N generic params while M
/// detected". A bare reference with no args (`Map`, `node`) is the
/// all-`any?` default and stays valid; only a non-empty, wrong-count
/// argument list is flagged. Heads whose arity isn't a known generic
/// (non-generic types, generic params in scope, unresolved names) are
/// left alone — the arity oracle only speaks for generic decls.
fn collect_generic_arity_diags(
    cur_module: &ModuleAnalysis,
    index: &ProjectIndex,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use crate::lower_type_ref::generic_arity_for;

    for (_idx, tr) in cur_module.hir.type_refs.iter() {
        if tr.params.is_empty() {
            continue;
        }
        let name = cur_module.hir.idents[tr.name].symbol;
        let Some(expected) = generic_arity_for(
            name,
            &cur_module.hir,
            &cur_module.analysis.type_decls,
            index,
        ) else {
            continue;
        };
        let got = tr.params.len();
        if got == expected {
            continue;
        }
        let head = &index.symbols[name];
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "generic-arity-mismatch",
            message: format!(
                "`{head}` expects {expected} generic argument{}, but got {got}",
                if expected == 1 { "" } else { "s" },
            ),
            byte_range: tr.byte_range.clone(),
            category: DiagCategory::TypeRelation,
        });
    }
}

/// Object-construction shape walker.
///
/// GreyCat's `T { … }` syntax carries two implicit construction shapes
/// the grammar can't disambiguate:
///
/// - `Array<T> { e1, e2, … }` — positional, any arity. `[e1, …]` is
///   sugar for the same thing.
/// - `node<T> { v }` — positional, at most one element.
///
/// Every other type — user-declared types, `Map`, `Tuple`, `Buffer`,
/// the other node-tag family members — must use the named form
/// (`T { field: value }`). Positional usage is rejected by the
/// runtime. The check runs here because it needs the `arena.builtins`
/// identities for `node` and `Array`, which `parse_diagnostics`
/// doesn't see.
#[allow(clippy::mutable_key_type, clippy::too_many_arguments)]
fn collect_object_construction_diags(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    arena: &mut TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    cur_uri: &Uri,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::Severity;
    use greycat_analyzer_hir::hir::Expr;

    let Some(cur_module) = modules.get(cur_uri) else {
        return;
    };
    for (obj_expr_id, expr) in cur_module.hir.exprs.iter() {
        // Only the positional form (`Foo { a, b }`) is this pass's
        // concern — named construction (`Foo { k: v }`, including
        // `Map`) is a different HIR variant and handled elsewhere.
        let Expr::PositionalObject(obj_expr) = expr else {
            continue;
        };
        // Empty `T {}` is a valid default-init for every head EXCEPT a
        // `node<T>` whose element type is non-nullable: the runtime
        // rejects `node<int> {}` with "non nullable node requires an
        // initial value". A nullable element (`node<int?> {}`) defaults
        // to null and stays valid.
        if obj_expr.fields.is_empty() {
            if let Some(obj_ty) = cur_module.analysis.expr_types.get(&obj_expr_id).copied() {
                let node_elem = match &arena.get(obj_ty).kind {
                    TypeKind::Generic { tpl, args } => Some((*tpl, args.first().copied())),
                    _ => None,
                };
                if let Some((tpl, Some(elem_ty))) = node_elem
                    && arena.is_node(tpl)
                    && !arena.get(elem_ty).nullable
                {
                    let elem_disp = display_type_for_module(
                        arena,
                        index,
                        decl_registry,
                        elem_ty,
                        Some(cur_uri),
                    );
                    diags.push(SemanticDiagnostic {
                        severity: Severity::Error,
                        code: "node-init-required",
                        message: format!(
                            "`node<{elem_disp}>` requires an initial value \
                             (`{elem_disp}` is not nullable)"
                        ),
                        byte_range: obj_expr.byte_range.clone(),
                        category: DiagCategory::TypeRelation,
                    });
                }
            }
            continue;
        }
        // Dispatch on the already-settled outer type identity.
        let Some(obj_ty) = cur_module.analysis.expr_types.get(&obj_expr_id).copied() else {
            continue;
        };
        // `geo { lat, lng }` — exactly two int/float components (int
        // coerces to float, `geo { 1, 2 }` ≡ `geo { 1.0, 2.0 }`). Checked
        // here, before the fixed-tuple / generic positional dispatch
        // below, since `geo` has its own arity rule.
        if obj_ty == arena.builtins.geo {
            if obj_expr.fields.len() != 2 {
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "geo-init-arity",
                    message: "`geo` requires exactly two positional initializers (lat, lng)"
                        .to_string(),
                    byte_range: obj_expr.byte_range.clone(),
                    category: DiagCategory::TypeRelation,
                });
            } else {
                let slot_desc = "element type `float`".to_string();
                for value in obj_expr.fields.iter() {
                    check_construction_value_against_slot(
                        cur_module,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        *value,
                        arena.builtins.float,
                        "element-type-mismatch",
                        &slot_desc,
                        diags,
                    );
                }
            }
            continue;
        }
        // The element type for `Array<T>` / `node<T>` is the head's
        // single generic arg, already settled on `obj_ty` (bare `Array`
        // expands to `Array<any?>`, so the check is a no-op there).
        let (head_decl, elem_ty) = match &arena.get(obj_ty).kind {
            TypeKind::Generic { tpl, args } => (*tpl, args.first().copied()),
            TypeKind::Type(decl) => (*decl, None),
            _ => continue,
        };
        if head_decl == arena.builtins.array_key {
            if let Some(elem_ty) = elem_ty {
                let slot_desc = format!(
                    "element type `{}`",
                    display_type_for_module(arena, index, decl_registry, elem_ty, Some(cur_uri)),
                );
                for value in obj_expr.fields.iter() {
                    check_construction_value_against_slot(
                        cur_module,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        *value,
                        elem_ty,
                        "element-type-mismatch",
                        &slot_desc,
                        diags,
                    );
                }
            }
            continue;
        }
        if arena.is_node(head_decl) {
            if obj_expr.fields.len() > 1 {
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "node-init-arity",
                    message: "`node` accepts at most one positional initializer".to_string(),
                    byte_range: obj_expr.byte_range.clone(),
                    category: DiagCategory::TypeRelation,
                });
            } else if let Some(elem_ty) = elem_ty {
                let slot_desc = format!(
                    "element type `{}`",
                    display_type_for_module(arena, index, decl_registry, elem_ty, Some(cur_uri)),
                );
                for value in obj_expr.fields.iter() {
                    check_construction_value_against_slot(
                        cur_module,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        *value,
                        elem_ty,
                        "element-type-mismatch",
                        &slot_desc,
                        diags,
                    );
                }
            }
            continue;
        }

        // The other node-tag family members accept NO initializer at all
        // — only the empty default-init `T {}` is valid (handled by the
        // empty-fields short-circuit above). Unlike `node` (one element)
        // and `Array` (any arity), any content here is a runtime error,
        // and it's neither positional nor named — so we flag it directly
        // instead of falling through to the "use named form" suggestion.
        if arena.is_node_list(head_decl)
            || arena.is_node_time(head_decl)
            || arena.is_node_geo(head_decl)
            || arena.is_node_index(head_decl)
        {
            let tr = &cur_module.hir.type_refs[obj_expr.ty];
            let head_name = &index.symbols[cur_module.hir.idents[tr.name].symbol];
            diags.push(SemanticDiagnostic {
                severity: Severity::Error,
                code: "node-tag-no-init",
                message: format!("`{head_name}` does not accept any initializer"),
                byte_range: obj_expr.byte_range.clone(),
                category: DiagCategory::TypeRelation,
            });
            continue;
        }

        // v7 fixed-shape tuple natives. Each slot is `Some` only when
        // the loaded stdlib is v7 (slots stay `None` on v8 / no-stdlib
        // projects, so the comparisons all miss and the loop falls
        // through to the named-form rule below). Contract per type:
        // exact positional arity + every element typed as one of
        // `accepted`. The float variants asymmetrically accept `int`
        // (runtime coerces `int → float` — verified against
        // `greycat run` v7.8); the int variants stay strict (runtime
        // rejects `float → int` even for literals).
        // Each accepted element is a `(Builtins selector, display name)`
        // pair: the selector resolves the canonical `Type(core::X)` for
        // the membership check, the name feeds the diagnostic message.
        type Accepted<'a> = &'a [(TypeId, &'static str)];
        let resolve_tuple =
            |name: &str| index.resolve_type(decl_registry, None, index.symbols.intern(name));
        let fixed_tuples: [(Option<ItemKey>, usize, Accepted, &str); 7] = [
            (resolve_tuple("t2"), 2, &[(arena.builtins.int, "int")], "t2"),
            (
                resolve_tuple("t2f"),
                2,
                &[(arena.builtins.float, "float"), (arena.builtins.int, "int")],
                "t2f",
            ),
            (resolve_tuple("t3"), 3, &[(arena.builtins.int, "int")], "t3"),
            (
                resolve_tuple("t3f"),
                3,
                &[(arena.builtins.float, "float"), (arena.builtins.int, "int")],
                "t3f",
            ),
            (resolve_tuple("t4"), 4, &[(arena.builtins.int, "int")], "t4"),
            (
                resolve_tuple("t4f"),
                4,
                &[(arena.builtins.float, "float"), (arena.builtins.int, "int")],
                "t4f",
            ),
            (
                resolve_tuple("str"),
                1,
                &[(arena.builtins.string, "String")],
                "str",
            ),
        ];
        let mut matched_v7 = false;
        for &(slot, arity, accepted, type_name) in &fixed_tuples {
            if slot != Some(head_decl) {
                continue;
            }
            matched_v7 = true;
            if obj_expr.fields.len() != arity {
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "fixed-tuple-arity",
                    message: format!(
                        "`{type_name}` requires exactly {arity} positional initializer{plural} (got {got})",
                        plural = if arity == 1 { "" } else { "s" },
                        got = obj_expr.fields.len(),
                    ),
                    byte_range: obj_expr.byte_range.clone(),
                    category: DiagCategory::TypeRelation,
                });
            } else {
                for value in obj_expr.fields.iter() {
                    let Some(val_ty) = cur_module.analysis.expr_types.get(value).copied() else {
                        continue;
                    };
                    let ok = accepted
                        .iter()
                        .any(|(expected_ty, _)| val_ty == *expected_ty);
                    if !ok {
                        let accepted_msg = accepted
                            .iter()
                            .map(|(_, name)| format!("`{name}`"))
                            .collect::<Vec<_>>()
                            .join(" or ");
                        diags.push(SemanticDiagnostic {
                            severity: Severity::Error,
                            code: "fixed-tuple-element-type",
                            message: format!("`{type_name}` element must be {accepted_msg}"),
                            byte_range: cur_module.hir.exprs[*value].byte_range(),
                            category: DiagCategory::TypeRelation,
                        });
                    }
                }
            }
            break;
        }
        if matched_v7 {
            continue;
        }

        let tr = &cur_module.hir.type_refs[obj_expr.ty];
        let head_name = &index.symbols[cur_module.hir.idents[tr.name].symbol];
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "positional-object-init",
            message: format!(
                "`{head_name}` does not accept positional initializers; use named form `{head_name} {{ field: value }}`"
            ),
            byte_range: obj_expr.byte_range.clone(),
            category: DiagCategory::TypeRelation,
        });
    }
}

#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_call_target(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    callee: Idx<Expr>,
) -> Option<(Option<Uri>, Idx<Decl>)> {
    use crate::analyzer::MemberDef;
    use crate::resolver::Definition;

    let callee_expr = &cur.hir.exprs[callee];
    match callee_expr {
        Expr::Ident { name: name_idx, .. } => match cur.resolutions.lookup(*name_idx)? {
            Definition::Decl(decl_id) => {
                if matches!(cur.hir.decls[decl_id], Decl::Fn(_)) {
                    Some((None, decl_id))
                } else {
                    None
                }
            }
            Definition::ProjectDecl { uri, decl } => {
                let m = modules.get(&uri)?;
                if matches!(m.hir.decls[decl], Decl::Fn(_)) {
                    Some((Some(uri), decl))
                } else {
                    None
                }
            }
            _ => None,
        },
        Expr::Static(s) => resolve_member_call(cur, s.property.ident()),
        Expr::Member(m) | Expr::Arrow(m) => resolve_member_call(cur, m.property.ident()),
        Expr::QualifiedStatic { chain, .. } => {
            let (uri, _type_decl_id, target) = resolve_qualified_chain(modules, index, cur, chain)?;
            match target {
                QualifiedTarget::Member(MemberDef::Method(decl_id)) => Some((Some(uri), decl_id)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Shared resolution for `Expr::Static` / `Expr::Member` / `Expr::Arrow` callees.
/// All three populate the same `member_uses` / `foreign_member_uses` maps during
/// the analyzer's `resolve_member` pass, so dispatch on the property ident.
fn resolve_member_call(
    cur: &ModuleAnalysis,
    property: Idx<Ident>,
) -> Option<(Option<Uri>, Idx<Decl>)> {
    use crate::analyzer::MemberDef;
    if let Some(MemberDef::Method(decl_id)) = cur.analysis.member_lookup(property) {
        return Some((None, decl_id));
    }
    if let Some(foreign) = cur.analysis.foreign_member_lookup(property)
        && let MemberDef::Method(decl_id) = foreign.member
    {
        return Some((Some(foreign.uri.clone()), decl_id));
    }
    None
}

// P15.8
/// Qualified-chain resolution result, used by
/// `resolve_qualified_chain` (the only remaining caller after
/// cleanup). Discriminates "the chain landed on an attr / method"
/// from "the chain landed on an enum variant" so call-target
/// resolution can dispatch correctly.
enum QualifiedTarget {
    Member(crate::analyzer::MemberDef),
    /// Enum-variant access — `module::Foo::a` where `Foo` is an
    /// enum decl and `a` matches one of its variants.
    EnumVariant,
}

/// Walk a `module::Type::member` chain and resolve each segment.
/// Returns (foreign_module_uri, type_decl_id, target). Length
/// must be exactly 3; other lengths return `None`.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_qualified_chain(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    chain: &[Idx<Ident>],
) -> Option<(Uri, Idx<Decl>, QualifiedTarget)> {
    use crate::analyzer::MemberDef;
    if chain.len() != 3 {
        return None;
    }
    let module_name_sym = cur.hir.idents[chain[0]].symbol;
    let type_name_sym = cur.hir.idents[chain[1]].symbol;
    let member_name_sym = cur.hir.idents[chain[2]].symbol;
    let module_uri = index.module_names.get(&module_name_sym)?.clone();
    let foreign = modules.get(&module_uri)?;
    let foreign_root = foreign.hir.module.as_ref()?;
    // Look for the named decl — could be a `type` or `enum`.
    let mut found: Option<Idx<Decl>> = None;
    for decl_id in &foreign_root.decls {
        let name_sym = match &foreign.hir.decls[*decl_id] {
            Decl::Type(td) => foreign.hir.idents[td.name].symbol,
            Decl::Enum(ed) => foreign.hir.idents[ed.name].symbol,
            _ => continue,
        };
        if name_sym == type_name_sym {
            found = Some(*decl_id);
            break;
        }
    }
    let type_decl_id = found?;
    match &foreign.hir.decls[type_decl_id] {
        Decl::Enum(ed) => {
            for f in &ed.fields {
                if foreign.hir.idents[foreign.hir.enum_fields[*f].name].symbol == member_name_sym {
                    return Some((module_uri, type_decl_id, QualifiedTarget::EnumVariant));
                }
            }
            None
        }
        Decl::Type(td) => {
            for attr_id in &td.attrs {
                if foreign.hir.idents[foreign.hir.type_attrs[*attr_id].name].symbol
                    == member_name_sym
                {
                    return Some((
                        module_uri,
                        type_decl_id,
                        QualifiedTarget::Member(MemberDef::Attr(*attr_id)),
                    ));
                }
            }
            for method_id in &td.methods {
                let Decl::Fn(m) = &foreign.hir.decls[*method_id] else {
                    continue;
                };
                if foreign.hir.idents[m.name].symbol == member_name_sym {
                    return Some((
                        module_uri,
                        type_decl_id,
                        QualifiedTarget::Member(MemberDef::Method(*method_id)),
                    ));
                }
            }
            None
        }
        _ => None,
    }
}

/// Walk one module's HIR and emit every type-relation diagnostic
///
/// Per-module typed-lint runner extracted out of [`ProjectAnalysis::run_typed_lints`]
/// so the parallel and serial paths share one body and a future
/// regression doesn't drift between them.
///
/// Reads `arena` + `index` immutably; writes only to `module.lints` /
/// `module.directives`. `doc_data` is consulted for the `catch-empty-parens`
/// lint, which needs the source text + parsed tree (the HIR drops the
/// empty `()` shape).
#[allow(clippy::mutable_key_type, clippy::too_many_arguments)]
fn run_typed_lints_for_module(
    uri: &Uri,
    module: &mut ModuleAnalysis,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    bypass: bool,
    enabled_rules: &FxHashSet<String>,
    doc_data: &FxHashMap<Uri, (String, Tree)>,
) {
    // A rule fires if any project-wide opt-in surface
    // (CLI `--on`, entrypoint `@lint_on`) enables it. Module-scope
    // pragmas no longer apply: P40.5 rejects them as
    // `lint-pragma-outside-entrypoint`. So the gate is just the
    // project-wide set.
    let no_breakpoint_on = enabled_rules.contains("no-breakpoint");
    module.lints.retain(|l| {
        !matches!(
            l.rule,
            "arrow-on-non-deref"
                | "possibly-null"
                | "redundant-nullable-access"
                | "redundant-non-null-assertion"
                | "redundant-coalesce"
                | "infer-return-type"
                | "unused-suppression"
                | "unreachable"
                | "non-exhaustive"
                | "catch-empty-parens"
                | "redundant-semicolon"
                | "no-breakpoint"
        ) && !SURFACED_RULES.contains(&l.rule)
    });
    if let Some((text, tree)) = doc_data.get(uri) {
        lint_catch_empty_parens(
            text,
            tree.root_node(),
            &mut module.directives,
            bypass,
            &mut module.lints,
        );
        lint_redundant_semicolon(
            text,
            tree.root_node(),
            &mut module.directives,
            bypass,
            &mut module.lints,
        );
        // Advisory, default-off. Runs when any opt-in
        // surface (CLI `--on=no-breakpoint`, entrypoint `@lint_on(...)`,
        // or this module's own `@lint_on(...)`) has enabled it.
        if no_breakpoint_on {
            lint_no_breakpoint(
                tree.root_node(),
                &mut module.directives,
                bypass,
                &mut module.lints,
            );
        }
    }
    lint_arrow_on_non_deref_with_directives(
        &module.hir,
        &module.analysis,
        arena,
        index,
        decl_registry,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    lint_nullability_with_directives(
        &module.hir,
        &index.symbols,
        &module.analysis,
        arena,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    lint_inferred_return_type_with_directives(
        &module.hir,
        &module.analysis,
        arena,
        index,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    lint_unreachable_with_directives(
        &module.hir,
        &module.analysis,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    lint_non_exhaustive_with_directives(
        &index.symbols,
        &module.analysis,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    lint_surfaced_with_directives(
        &module.analysis,
        &mut module.lints,
        &mut module.directives,
        bypass,
    );
    if !bypass {
        lint_unused_suppressions(&mut module.directives, &mut module.lints);
    }

    // Final project / module-pragma filter.
    // The `disabled_rules` / `pragma_disabled_rules` filter
    // doesn't live in this function. Subsequent passes
    // (`stage_compute_qualified_refs`) re-emit lints, so the policy
    // filter has to land in one place after every emission settles —
    // see [`ProjectAnalysis::apply_rule_policy`], called at the tail
    // of `analyze_staged` and `invalidate`.
}

/// Any newly-needed declared-side TypeIds are minted into it
/// alongside everything else, which is fine because the arena is
/// append-only and intern-collapsed.
fn validate_module_type_relations(
    module: &ModuleAnalysis,
    cur_uri: &Uri,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::hir::Decl;

    let hir = &module.hir;
    let analysis = &module.analysis;

    let Some(top) = hir.module.as_ref() else {
        return;
    };
    // Top-level decls have no enclosing-type generics in scope.
    let no_outer_generics: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
    for d_id in &top.decls {
        validate_decl(
            hir,
            analysis,
            cur_uri,
            index,
            decl_registry,
            arena,
            arena.builtins.bool_,
            &hir.decls[*d_id],
            &no_outer_generics,
            diags,
        );
    }

    /// `outer_generics` carries the enclosing type's generic params
    /// (`GenericOwner::Type`) when validating a method body, empty for
    /// top-level fns. Merged with the fn's own generics so the declared
    /// return type lowers to the same shape the body / signature use.
    #[allow(clippy::too_many_arguments)]
    fn validate_decl(
        hir: &Hir,
        analysis: &AnalysisResult,
        cur_uri: &Uri,
        index: &ProjectIndex,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        bool_t: TypeId,
        decl: &Decl,
        outer_generics: &FxHashMap<Symbol, GenericOwner>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        match decl {
            Decl::Fn(fnd) => {
                // Lower the declared return type WITH the fn's generics
                // (plus any enclosing-type generics for methods) in scope
                // so `fn wrap<T>(): Array<T>` keeps `Array<T>`
                // (`GenericParam`) instead of collapsing `T` to `any?` —
                // which mismatched the body's `Array<T>`-typed `return`
                // and produced a spurious return-type-mismatch. Mirrors
                // signature lowering (S7-S11), so the body validation
                // compares against the same shape the signature carries.
                let return_ty = fnd.return_type.map(|t| {
                    let mut generics_in_scope = outer_generics.clone();
                    let own_owner = GenericOwner::Function(hir.idents[fnd.name].symbol);
                    for g in &fnd.generics {
                        generics_in_scope.insert(hir.idents[*g].symbol, own_owner);
                    }
                    lower_type_ref_project(
                        hir,
                        t,
                        arena,
                        index,
                        decl_registry,
                        &generics_in_scope,
                        Some(cur_uri),
                    )
                });
                if let Some(body) = fnd.body {
                    validate_stmt(
                        hir,
                        analysis,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        bool_t,
                        body,
                        return_ty,
                        diags,
                    );
                    // The runtime never validates the return type and
                    // implicitly returns `null` when control falls off
                    // the end of a body. Enforce the signature contract:
                    // a reachable end-of-body implicitly returns `null`,
                    // so flag it when `null` doesn't satisfy the declared
                    // return type. Nullable returns accept the implicit
                    // null and are fine even with a fall-through path.
                    if let (Some(ret_ty), Some(ret_tref)) = (return_ty, fnd.return_type) {
                        let null_ty = arena.null();
                        if !is_assignable_to_with_index(
                            index,
                            decl_registry,
                            arena,
                            null_ty,
                            ret_ty,
                        ) && !crate::reachability::stmt_diverges_with_analysis(
                            hir, analysis, body,
                        ) {
                            diags.push(SemanticDiagnostic {
                                severity: Severity::Error,
                                code: "missing-return",
                                message: format!(
                                    "function may reach the end of its body without \
                                     returning a value of type `{}`",
                                    display_type(arena, decl_registry, &index.symbols, ret_ty),
                                ),
                                byte_range: hir.type_refs[ret_tref].byte_range.clone(),
                                category: DiagCategory::TypeRelation,
                            });
                        }
                    }
                }
            }
            Decl::Type(td) => {
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    if let (Some(decl_ref), Some(init)) = (attr.ty, attr.init)
                        && let Some(declared_ty) = lower_type_ref_id(
                            hir,
                            decl_ref,
                            &analysis.registry,
                            index,
                            decl_registry,
                            &analysis.type_decls,
                            arena,
                            Some(cur_uri),
                        )
                    {
                        check_assign(
                            analysis,
                            index,
                            decl_registry,
                            arena,
                            init,
                            declared_ty,
                            "attribute initializer",
                            "declared type",
                            attr.byte_range.clone(),
                            diags,
                        );
                    }
                }
                // Method bodies see the enclosing type's generics
                // (`GenericOwner::Type`) — so `type Box<T> { fn dup():
                // Array<T> }` validates the body's `Array<T>` return
                // against `Array<T>`, not a collapsed `Array<any?>`.
                let type_owner = GenericOwner::Type(hir.idents[td.name].symbol);
                let type_generics: FxHashMap<Symbol, GenericOwner> = td
                    .generics
                    .iter()
                    .map(|g| (hir.idents[*g].symbol, type_owner))
                    .collect();
                for m in &td.methods {
                    validate_decl(
                        hir,
                        analysis,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        bool_t,
                        &hir.decls[*m],
                        &type_generics,
                        diags,
                    );
                }
            }
            Decl::Var(vd) => {
                if let (Some(decl_ref), Some(init)) = (vd.ty, vd.init)
                    && let Some(declared_ty) = lower_type_ref_id(
                        hir,
                        decl_ref,
                        &analysis.registry,
                        index,
                        decl_registry,
                        &analysis.type_decls,
                        arena,
                        Some(cur_uri),
                    )
                {
                    check_assign(
                        analysis,
                        index,
                        decl_registry,
                        arena,
                        init,
                        declared_ty,
                        "initializer",
                        "declared type",
                        vd.byte_range.clone(),
                        diags,
                    );
                }
            }
            Decl::Enum(_) | Decl::Pragma(_) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_block(
        hir: &Hir,
        analysis: &AnalysisResult,
        cur_uri: &Uri,
        index: &ProjectIndex,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        bool_t: TypeId,
        block: &BlockStmt,
        return_ty: Option<TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        for s in &block.stmts {
            validate_stmt(
                hir,
                analysis,
                cur_uri,
                index,
                decl_registry,
                arena,
                bool_t,
                *s,
                return_ty,
                diags,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_stmt(
        hir: &Hir,
        analysis: &AnalysisResult,
        cur_uri: &Uri,
        index: &ProjectIndex,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        bool_t: TypeId,
        stmt_id: Idx<Stmt>,
        return_ty: Option<TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        use greycat_analyzer_hir::hir::{
            AssignStmt, AtStmt, DoWhileStmt, ForInStmt, ForStmt, IfStmt, LocalVar, Stmt, TryStmt,
            WhileStmt,
        };
        match &hir.stmts[stmt_id] {
            Stmt::Block(b) => validate_block(
                hir,
                analysis,
                cur_uri,
                index,
                decl_registry,
                arena,
                bool_t,
                b,
                return_ty,
                diags,
            ),
            Stmt::Var(LocalVar { ty, init, .. }) => {
                if let (Some(decl_ref), Some(init_id)) = (ty, init)
                    && let Some(declared_ty) = lower_type_ref_id(
                        hir,
                        *decl_ref,
                        &analysis.registry,
                        index,
                        decl_registry,
                        &analysis.type_decls,
                        arena,
                        Some(cur_uri),
                    )
                {
                    let r = expr_byte_range(hir, *init_id);
                    check_assign(
                        analysis,
                        index,
                        decl_registry,
                        arena,
                        *init_id,
                        declared_ty,
                        "var initializer",
                        "declared type",
                        r,
                        diags,
                    );
                }
            }
            Stmt::Assign(AssignStmt {
                target,
                value,
                byte_range,
                ..
            }) => {
                if let Some(target_ty) = analysis.expr_types.get(target).copied() {
                    check_assign(
                        analysis,
                        index,
                        decl_registry,
                        arena,
                        *value,
                        target_ty,
                        "value",
                        "target",
                        byte_range.clone(),
                        diags,
                    );
                }
            }
            Stmt::If(IfStmt {
                condition,
                then_branch,
                else_branch,
                ..
            }) => {
                check_bool(
                    analysis,
                    arena,
                    index,
                    decl_registry,
                    *condition,
                    bool_t,
                    "if condition",
                    hir,
                    diags,
                );
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    then_branch,
                    return_ty,
                    diags,
                );
                if let Some(eb) = else_branch {
                    validate_stmt(
                        hir,
                        analysis,
                        cur_uri,
                        index,
                        decl_registry,
                        arena,
                        bool_t,
                        *eb,
                        return_ty,
                        diags,
                    );
                }
            }
            Stmt::While(WhileStmt {
                condition, body, ..
            }) => {
                check_bool(
                    analysis,
                    arena,
                    index,
                    decl_registry,
                    *condition,
                    bool_t,
                    "while condition",
                    hir,
                    diags,
                );
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    body,
                    return_ty,
                    diags,
                );
            }
            Stmt::DoWhile(DoWhileStmt {
                condition, body, ..
            }) => {
                check_bool(
                    analysis,
                    arena,
                    index,
                    decl_registry,
                    *condition,
                    bool_t,
                    "do-while condition",
                    hir,
                    diags,
                );
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    body,
                    return_ty,
                    diags,
                );
            }
            Stmt::For(ForStmt {
                condition, body, ..
            }) => {
                if let Some(c) = condition {
                    check_bool(
                        analysis,
                        arena,
                        index,
                        decl_registry,
                        *c,
                        bool_t,
                        "for condition",
                        hir,
                        diags,
                    );
                }
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    body,
                    return_ty,
                    diags,
                );
            }
            Stmt::ForIn(ForInStmt { body, .. }) => {
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    body,
                    return_ty,
                    diags,
                );
            }
            Stmt::Try(TryStmt {
                try_block,
                catch_block,
                ..
            }) => {
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    try_block,
                    return_ty,
                    diags,
                );
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    catch_block,
                    return_ty,
                    diags,
                );
            }
            Stmt::At(AtStmt { block, .. }) => {
                validate_block(
                    hir,
                    analysis,
                    cur_uri,
                    index,
                    decl_registry,
                    arena,
                    bool_t,
                    block,
                    return_ty,
                    diags,
                );
            }
            Stmt::Return(r) if r.value.is_some() => {
                let v = r.value.unwrap();
                if let Some(rt) = return_ty {
                    let r = expr_byte_range(hir, v);
                    check_assign(
                        analysis,
                        index,
                        decl_registry,
                        arena,
                        v,
                        rt,
                        "return value",
                        "declared return type",
                        r.clone(),
                        diags,
                    );
                    // P-erasure: returning a value whose materialized type
                    // fits the declared return but whose runtime-erased
                    // shape doesn't — the runtime throws `wrong return
                    // type`. Guard on materialized-assignable so we don't
                    // double-fire with `check_assign`'s type-mismatch.
                    if let Some(runtime_ty) = analysis.expr_runtime_types.get(&v).copied()
                        && let Some(value_ty) = analysis.expr_types.get(&v).copied()
                        && is_assignable_to_with_index(index, decl_registry, arena, value_ty, rt)
                        && !is_assignable_to_with_index(index, decl_registry, arena, runtime_ty, rt)
                    {
                        let want =
                            display_type(arena, decl_registry, &index.symbols, rt).to_string();
                        push_generic_erasure_diag(
                            diags,
                            arena,
                            index,
                            decl_registry,
                            cur_uri,
                            runtime_ty,
                            format!("the declared return type `{want}`"),
                            r,
                        );
                    }
                }
            }
            Stmt::Return(r) => {
                // Bare `return;` is `return null;`. The runtime throws
                // "wrong return type ... null found while none nullable
                // expected" when the path runs; surface the same
                // `type-mismatch` the valued path emits for `return null;`.
                if let Some(rt) = return_ty {
                    let null_ty = arena.null();
                    if !is_assignable_to_with_index(index, decl_registry, arena, null_ty, rt) {
                        diags.push(SemanticDiagnostic {
                            severity: Severity::Error,
                            code: "type-mismatch",
                            message: format!(
                                "return value of type `null` is not assignable to \
                                 declared return type `{}`",
                                display_type(arena, decl_registry, &index.symbols, rt),
                            ),
                            byte_range: r.byte_range.clone(),
                            category: DiagCategory::TypeRelation,
                        });
                    }
                }
            }
            Stmt::Expr(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Breakpoint(_)
            | Stmt::Throw(_) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_assign(
        analysis: &AnalysisResult,
        index: &ProjectIndex,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        value_id: Idx<Expr>,
        declared_ty: TypeId,
        value_label: &str,
        target_label: &str,
        range: std::ops::Range<usize>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        let Some(value_ty) = analysis.expr_types.get(&value_id).copied() else {
            return;
        };
        if is_assignable_to_with_index(index, decl_registry, arena, value_ty, declared_ty) {
            return;
        }
        let got = display_type(arena, decl_registry, &index.symbols, value_ty);
        let want = display_type(arena, decl_registry, &index.symbols, declared_ty);
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "type-mismatch",
            message: format!(
                "{value_label} of type `{got}` is not assignable to {target_label} `{want}`"
            ),
            byte_range: range,
            category: DiagCategory::TypeRelation,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn check_bool(
        analysis: &AnalysisResult,
        arena: &TypeArena,
        index: &ProjectIndex,
        decl_registry: &DeclRegistry,
        expr_id: Idx<Expr>,
        bool_t: TypeId,
        label: &'static str,
        hir: &Hir,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        let Some(ty) = analysis.expr_types.get(&expr_id).copied() else {
            return;
        };
        if arena.is_assignable_to(ty, bool_t) {
            return;
        }
        let got = display_type(arena, decl_registry, &index.symbols, ty);
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "non-bool-condition",
            message: format!("{label} must be `bool`, got `{got}`"),
            byte_range: expr_byte_range(hir, expr_id),
            category: DiagCategory::TypeRelation,
        });
    }

    fn expr_byte_range(hir: &Hir, expr_id: Idx<Expr>) -> std::ops::Range<usize> {
        hir.exprs[expr_id].byte_range()
    }
}

/// Signature-lowering view for the shared `TypeRef` ladder. No local
/// registry (signature lowering mints into the index, not a per-module
/// registry); generic params come from the flat in-scope map.
struct SigLowerEnv<'a> {
    hir: &'a Hir,
    index: &'a ProjectIndex,
    decl_registry: &'a DeclRegistry,
    current_uri: Option<&'a Uri>,
    generics_in_scope: &'a FxHashMap<Symbol, GenericOwner>,
}

impl TypeRefLowering for SigLowerEnv<'_> {
    fn hir(&self) -> &Hir {
        self.hir
    }
    fn index(&self) -> &ProjectIndex {
        self.index
    }
    fn decl_registry(&self) -> &DeclRegistry {
        self.decl_registry
    }
    fn current_uri(&self) -> Option<&Uri> {
        self.current_uri
    }
    fn lookup_generic(&self, name: Symbol) -> Option<GenericOwner> {
        self.generics_in_scope.get(&name).copied()
    }
    fn generic_arity_for(&self, name: Symbol) -> Option<usize> {
        // Signature lowering's arity check requires the decl already be
        // in `decl_registry` (via `resolve_type`), unlike the body
        // walk's index-walking `generic_arity_for`.
        if let Some(n) = self
            .index
            .resolve_type(self.decl_registry, self.current_uri, name)
            .and_then(|item| self.index.type_members.get(&item))
            .map(|m| m.generics.len())
            .filter(|n| *n > 0)
        {
            return Some(n);
        }
        // Bare node-tag raw-form arity even without `core.gcl` loaded.
        Builtins::node_tag_arity(&self.index.symbols[name])
    }
}

/// Project-wide TypeRef lowerer used by signature lowering. Uses the
/// project index instead of a per-module registry, so foreign type
/// names resolve in the shared arena. `generics_in_scope` maps the
/// current type / fn's generic params to their `GenericOwner`.
fn lower_type_ref_project(
    hir: &Hir,
    type_ref: Idx<TypeRef>,
    arena: &mut TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    generics_in_scope: &FxHashMap<Symbol, GenericOwner>,
    current_uri: Option<&Uri>,
) -> TypeId {
    let mut env = SigLowerEnv {
        hir,
        index,
        decl_registry,
        current_uri,
        generics_in_scope,
    };
    lower_type_ref::lower_type_ref_with(&mut env, arena, type_ref)
}

/// Validation-pass view for the shared `TypeRef` ladder. `lookup_local`
/// consults the validation `TypeRegistry` (the working clone of
/// `analysis.types`); generic params are not in scope here yet.
struct ValidateLowerEnv<'a> {
    hir: &'a Hir,
    index: &'a ProjectIndex,
    decl_registry: &'a DeclRegistry,
    current_uri: Option<&'a Uri>,
    registry: &'a TypeRegistry,
    type_decls: &'a FxHashMap<Symbol, Idx<Decl>>,
}

impl TypeRefLowering for ValidateLowerEnv<'_> {
    fn hir(&self) -> &Hir {
        self.hir
    }
    fn index(&self) -> &ProjectIndex {
        self.index
    }
    fn decl_registry(&self) -> &DeclRegistry {
        self.decl_registry
    }
    fn current_uri(&self) -> Option<&Uri> {
        self.current_uri
    }
    fn lookup_local(&self, name: Symbol) -> Option<TypeId> {
        self.registry.lookup(name)
    }
    fn generic_arity_for(&self, name: Symbol) -> Option<usize> {
        lower_type_ref::generic_arity_for(name, self.hir, self.type_decls, self.index)
    }
}

/// Look up a syntactic `TypeRef` and mint a corresponding `TypeId` into
/// `arena`, the validation-pass's working clone of `analysis.types`, so
/// new mints land where `is_assignable_to` can see them.
#[allow(clippy::too_many_arguments)]
fn lower_type_ref_id(
    hir: &Hir,
    type_ref: Idx<TypeRef>,
    registry: &TypeRegistry,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    type_decls: &FxHashMap<Symbol, Idx<Decl>>,
    arena: &mut TypeArena,
    current_uri: Option<&Uri>,
) -> Option<TypeId> {
    let mut env = ValidateLowerEnv {
        hir,
        index,
        decl_registry,
        current_uri,
        registry,
        type_decls,
    };
    Some(lower_type_ref::lower_type_ref_with(
        &mut env, arena, type_ref,
    ))
}

// P15.7
/// When a call's callee is a `Member` / `Arrow` access on a generic
/// receiver (`n.set(...)` where `n: node<int?>`), build the
/// `{ generic_param_name → concrete_shape }` map needed to substitute
/// the method's declared param types at validation time.
///
/// Returns an empty map when:
/// - the callee isn't a member access (e.g. bare `f(...)`),
/// - the receiver's settled type isn't a generic instantiation,
/// - we can't find the receiver type's `Decl::Type` in `fn_module`
///   (the foreign / containing module of the method).
///
/// **Why this is needed.** The validation pass's
/// `read_type_shape` produces `Named { name: "T", params: [] }` for
/// a method param `value: T` — it doesn't know `T` is a generic param
/// of the enclosing type. Without substitution, the call-arg
/// validator compares `int` (the arg) against `T` (literal),
/// surfaces "value of type `int` is not assignable to parameter
/// `value: T`", and the call appears broken to the user even though
/// the runtime accepts it cleanly. Substituting `T → int?` before
/// minting closes the gap.
type GenericsInScope = FxHashMap<Symbol, GenericOwner>;
type MethodSubst = FxHashMap<Symbol, TypeId>;

fn method_subst_from_receiver(
    arena: &TypeArena,
    cur_module: &ModuleAnalysis,
    fn_module: &ModuleAnalysis,
    index: &ProjectIndex,
    callee_expr: &greycat_analyzer_hir::hir::Expr,
) -> (GenericsInScope, MethodSubst) {
    use greycat_analyzer_hir::hir::Expr;
    let empty = || (GenericsInScope::default(), MethodSubst::default());
    let receiver_expr_id = match callee_expr {
        Expr::Member(m) | Expr::Arrow(m) => m.receiver,
        _ => return empty(),
    };
    let Some(receiver_ty) = cur_module
        .analysis
        .expr_types
        .get(&receiver_expr_id)
        .copied()
    else {
        return empty();
    };
    let recv = arena.get(receiver_ty);
    let (recv_name, recv_args): (&str, &[TypeId]) = match &recv.kind {
        TypeKind::Generic { tpl, args } => (&index.symbols[tpl.name], args.as_slice()),
        _ => return empty(),
    };
    let Some(module) = fn_module.hir.module.as_ref() else {
        return empty();
    };
    let mut owner_td: Option<&greycat_analyzer_hir::hir::TypeDecl> = None;
    for d_id in &module.decls {
        if let Decl::Type(td) = &fn_module.hir.decls[*d_id]
            && &index.symbols[fn_module.hir.idents[td.name].symbol] == recv_name
        {
            owner_td = Some(td);
            break;
        }
    }
    let Some(td) = owner_td else {
        return empty();
    };
    let owner = match index.symbol(recv_name) {
        Some(s) => GenericOwner::Type(s),
        None => return empty(),
    };
    let mut generics_in_scope: GenericsInScope = FxHashMap::default();
    let mut subst: MethodSubst = FxHashMap::default();
    for (i, gen_idx) in td.generics.iter().enumerate() {
        let gen_sym = fn_module.hir.idents[*gen_idx].symbol;
        generics_in_scope.insert(gen_sym, owner);
        if let Some(arg_id) = recv_args.get(i).copied() {
            subst.insert(gen_sym, arg_id);
        }
    }
    (generics_in_scope, subst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).unwrap()
    }

    #[test]
    fn analyze_project_covers_every_doc() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(): int { return 1; }\n",
            "p",
            false,
        );
        mgr.add_simple(
            uri("/proj/b.gcl"),
            "fn b(x: int): int { return x; }\n",
            "p",
            false,
        );

        let pa = ProjectAnalysis::analyze(&mgr);
        assert_eq!(pa.len(), 2);
        assert!(pa.module(&uri("/proj/a.gcl")).is_some());
        assert!(pa.module(&uri("/proj/b.gcl")).is_some());
    }

    #[test]
    fn module_analysis_carries_doc_lib() {
        // The lib name is cached on `ModuleAnalysis` at construction so
        // CLI / LSP consumers can filter lib-owned lints without a
        // SourceManager lookup at every emission site.
        let mut mgr = SourceManager::new();
        mgr.add_simple(uri("/proj/a.gcl"), "fn a() {}\n", "project", false);
        mgr.add_simple(uri("/proj/lib/std/core.gcl"), "fn b() {}\n", "std", false);
        let pa = ProjectAnalysis::analyze(&mgr);
        assert_eq!(pa.module(&uri("/proj/a.gcl")).unwrap().lib, "project");
        assert_eq!(
            pa.module(&uri("/proj/lib/std/core.gcl")).unwrap().lib,
            "std"
        );
    }

    #[test]
    fn invalidate_preserves_doc_lib() {
        // Re-running `invalidate` for a doc must keep the cached lib
        // — the name doesn't move between rebuilds.
        let mut mgr = SourceManager::new();
        mgr.add_simple(uri("/proj/lib/std/core.gcl"), "fn b() {}\n", "std", false);
        let mut pa = ProjectAnalysis::analyze(&mgr);
        mgr.add_simple(
            uri("/proj/lib/std/core.gcl"),
            "fn b(): int { return 1; }\n",
            "std",
            false,
        );
        pa.invalidate(&mgr, &uri("/proj/lib/std/core.gcl"));
        assert_eq!(
            pa.module(&uri("/proj/lib/std/core.gcl")).unwrap().lib,
            "std"
        );
    }

    #[test]
    fn shared_index_sees_types_from_every_module() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(uri("/proj/types.gcl"), "type Point {}\n", "p", false);
        mgr.add_simple(uri("/proj/main.gcl"), "fn f() {}\n", "p", false);

        let pa = ProjectAnalysis::analyze(&mgr);
        let point_sym = pa.index.symbols.lookup("Point").expect("Point interned");
        assert!(
            pa.index.has_name(point_sym),
            "shared index should know about Point declared in another module"
        );
    }

    #[test]
    fn invalidate_re_runs_changed_doc_only() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(): int { return 1; }\n",
            "p",
            false,
        );
        mgr.add_simple(
            uri("/proj/b.gcl"),
            "fn b(): int { return 1; }\n",
            "p",
            false,
        );

        let mut pa = ProjectAnalysis::analyze(&mgr);
        // Mutate a.gcl in the manager directly through `add` (overwrite).
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(): int { return \"oops\"; }\n",
            "p",
            false,
        );
        pa.invalidate(&mgr, &uri("/proj/a.gcl"));

        let a = pa.module(&uri("/proj/a.gcl")).expect("a in cache");
        assert!(
            a.analysis
                .diagnostics
                .iter()
                .any(|d| d.message.contains("declared return type")),
            "expected return-type mismatch on a.gcl after change, got {:?}",
            a.analysis.diagnostics
        );
        let b = pa.module(&uri("/proj/b.gcl")).expect("b stayed cached");
        assert!(
            b.analysis.diagnostics.is_empty(),
            "b.gcl shouldn't have grown new diagnostics"
        );
    }

    #[test]
    fn qualified_access_keeps_private_decl_alive() {
        // P14.9: a `private fn handler() {}` in helper.gcl is reachable
        // from main.gcl via `helper::handler()`. The unused-decl lint
        // should not flag it.
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/helper.gcl"),
            "private fn handler(): int { return 1; }\n",
            "p",
            false,
        );
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "fn main() { helper::handler(); }\n",
            "p",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let helper = pa.module(&uri("/proj/helper.gcl")).expect("helper module");
        assert!(
            !helper.lints.iter().any(|l| l.rule == "unused-decl"),
            "helper::handler should be marked alive by qualified ref: {:?}",
            helper.lints
        );
    }

    #[test]
    fn invalidate_drops_removed_uri() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(uri("/proj/a.gcl"), "fn a() {}\n", "p", false);
        let mut pa = ProjectAnalysis::analyze(&mgr);
        assert_eq!(pa.len(), 1);

        let removed = mgr.remove(&uri("/proj/a.gcl"));
        assert!(removed.is_some());
        pa.invalidate(&mgr, &uri("/proj/a.gcl"));
        assert_eq!(pa.len(), 0);
        assert!(pa.module(&uri("/proj/a.gcl")).is_none());
    }

    // P19.6
    /// Body-only edits skip the per-module signature
    /// re-walk by reusing the cached contributions. The cache is
    /// populated after the initial build and stays the same size
    /// after an unrelated body edit; the changed module's hash is
    /// re-validated but its cached contributions are reused
    /// (sig_hash unchanged).
    #[test]
    fn invalidate_body_only_edit_reuses_sig_cache() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(x: int): int { return x; }\n",
            "p",
            false,
        );
        mgr.add_simple(
            uri("/proj/b.gcl"),
            "type Pair { left: int; right: int; }\nfn b(): int { return 0; }\n",
            "p",
            false,
        );
        let mut pa = ProjectAnalysis::analyze(&mgr);
        assert_eq!(pa.sig_cache_len(), 2, "both modules cached after rebuild");
        let pair_id = pa
            .index
            .item_id_for(
                &uri("/proj/b.gcl"),
                pa.index.symbols.lookup("Pair").unwrap(),
            )
            .expect("Pair item id");
        let cached_attrs_before = pa
            .index
            .type_members
            .get(&pair_id)
            .map(|tm| tm.attr_types.len())
            .unwrap_or(0);

        // Body-only edit on a.gcl — `a`'s signature is identical.
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(x: int): int { return x + 1; }\n",
            "p",
            false,
        );
        pa.invalidate(&mgr, &uri("/proj/a.gcl"));
        assert_eq!(
            pa.sig_cache_len(),
            2,
            "cache size stable after body-only edit"
        );
        let cached_attrs_after = pa
            .index
            .type_members
            .get(&pair_id)
            .map(|tm| tm.attr_types.len())
            .unwrap_or(0);
        assert_eq!(
            cached_attrs_before, cached_attrs_after,
            "Pair's attr_types reapplied verbatim from the cache"
        );
    }

    // P19.6
    /// Signature edits invalidate the cache entry for
    /// the changed module. The new contributions overwrite the old
    /// in `index.fn_signatures`, so callers querying the new return
    /// type see it on the very next `module()` lookup.
    #[test]
    fn invalidate_signature_edit_refreshes_sig_cache() {
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(): int { return 1; }\n",
            "p",
            false,
        );
        let mut pa = ProjectAnalysis::analyze(&mgr);
        let a_id = pa
            .index
            .item_id_for(&uri("/proj/a.gcl"), pa.index.symbols.lookup("a").unwrap())
            .expect("a item id");
        assert!(pa.index.fn_signatures.contains_key(&a_id));

        // Change the return type — signature hash must differ.
        mgr.add_simple(
            uri("/proj/a.gcl"),
            "fn a(): String { return \"x\"; }\n",
            "p",
            false,
        );
        pa.invalidate(&mgr, &uri("/proj/a.gcl"));
        let sig = pa
            .index
            .fn_signatures
            .get(&a_id)
            .expect("a sig present after invalidate");
        let display = pa
            .display_type(sig.return_ty.expect("a has declared return type"))
            .to_string();
        assert!(
            display.contains("String"),
            "expected refreshed return type to be String, got {display:?}"
        );
    }

    /// Anchors the rule that cross-module / bare-Ident-call
    /// conditions don't surface a false-positive bool diagnostic.
    /// The analyzer's first pass returns `any` for such calls so it
    /// queues the check into `bool_check_conditions`; the post-pass
    /// `validate_condition_types` re-checks once
    /// `infer_cross_module_call_types` has settled the call's
    /// return type.
    #[test]
    fn condition_bool_check_uses_post_pass_types() {
        let mut mgr = SourceManager::new();
        // Cross-module: `is_something()` returns `bool` from another file.
        mgr.add_simple(
            uri("/proj/lib.gcl"),
            "native fn is_something(): bool;\n",
            "project",
            false,
        );
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "fn t() {\n    if (is_something()) {}\n}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let m = pa.module(&uri("/proj/main.gcl")).unwrap();
        let bool_diag = m
            .analysis
            .diagnostics
            .iter()
            .any(|d| d.message.contains("if condition must be `bool`"));
        assert!(
            !bool_diag,
            "no `if condition must be bool` diagnostic should fire when the callee returns bool, got: {:?}",
            m.analysis.diagnostics
        );

        // Real failure case: `if (1) {}` must still fire eagerly.
        let mut mgr2 = SourceManager::new();
        mgr2.add_simple(
            uri("/proj/main.gcl"),
            "fn t() {\n    if (1) {}\n}\n",
            "project",
            false,
        );
        let pa2 = ProjectAnalysis::analyze(&mgr2);
        let m2 = pa2.module(&uri("/proj/main.gcl")).unwrap();
        assert!(
            m2.analysis
                .diagnostics
                .iter()
                .any(|d| d.message.contains("if condition must be `bool`")),
            "expected eager bool-check diagnostic on `if (1)`, got: {:?}",
            m2.analysis.diagnostics
        );
    }

    // P41.1 — closure / reverse-index builders.

    /// Resolve a name through the project's `SymbolTable`, panic if absent.
    fn sym(pa: &ProjectAnalysis, name: &str) -> Symbol {
        pa.index
            .symbols
            .lookup(name)
            .unwrap_or_else(|| panic!("symbol `{name}` not interned"))
    }

    /// Collect a closure's concrete leaves as `&str` names so assertions
    /// don't depend on Symbol identity (which changes per `SymbolTable`).
    /// Tests place every type in `/proj/main.gcl` (module `main`), so
    /// the lookup is a direct ItemKey construction.
    fn closure_names<'a>(pa: &'a ProjectAnalysis, root: &str) -> Vec<&'a str> {
        let root_sym = sym(pa, root);
        let main_mod = pa.index.symbols.intern("main");
        let root_id = ItemKey::new(main_mod, root_sym);
        pa.index
            .subtype_closure
            .get(&root_id)
            .expect("closure entry present")
            .iter()
            .map(|id| &pa.index.symbols[id.name])
            .collect()
    }

    #[test]
    fn subtype_closure_concrete_only_and_descending() {
        // closure(X) includes X iff concrete; recurses into children
        // regardless of X's abstractness. The Shape root is abstract so
        // it doesn't include itself; Rect/Circle are concrete leaves
        // that ARE in closure(Shape). Concrete Rect with abstract
        // Quadrilateral parent: closure(Quadrilateral) = {Square, Rect}
        // (Rect concrete → self), but only when Rect extends
        // Quadrilateral. Keep the shape simple and assertable.
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "abstract type Shape {}\n\
             type Rect extends Shape {}\n\
             type Circle extends Shape {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);

        let mut leaves = closure_names(&pa, "Shape");
        leaves.sort();
        assert_eq!(leaves, vec!["Circle", "Rect"]);

        // Concrete-leaf closures contain just self.
        assert_eq!(closure_names(&pa, "Rect"), vec!["Rect"]);
        assert_eq!(closure_names(&pa, "Circle"), vec!["Circle"]);
    }

    #[test]
    fn subtype_closure_deeply_nested_abstract_chain() {
        // Animal → Mammal → Feline → Cat plus sibling branches at
        // every level. Exercises the recursive descent: closure(Animal)
        // spans the full leaf set; closure(Mammal) drops the Bird side;
        // closure(Feline) is just the Felines.
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "abstract type Animal {}\n\
             abstract type Mammal extends Animal {}\n\
             type Dog extends Mammal {}\n\
             type Horse extends Mammal {}\n\
             abstract type Feline extends Mammal {}\n\
             type Cat extends Feline {}\n\
             type Lynx extends Feline {}\n\
             type Tiger extends Feline {}\n\
             abstract type Bird extends Animal {}\n\
             type Eagle extends Bird {}\n\
             type Penguin extends Bird {}\n\
             type Sparrow extends Bird {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);

        let mut animal = closure_names(&pa, "Animal");
        animal.sort();
        assert_eq!(
            animal,
            vec![
                "Cat", "Dog", "Eagle", "Horse", "Lynx", "Penguin", "Sparrow", "Tiger"
            ],
        );
        let mut mammal = closure_names(&pa, "Mammal");
        mammal.sort();
        assert_eq!(mammal, vec!["Cat", "Dog", "Horse", "Lynx", "Tiger"]);
        let mut feline = closure_names(&pa, "Feline");
        feline.sort();
        assert_eq!(feline, vec!["Cat", "Lynx", "Tiger"]);
        let mut bird = closure_names(&pa, "Bird");
        bird.sort();
        assert_eq!(bird, vec!["Eagle", "Penguin", "Sparrow"]);
    }

    #[test]
    fn abstract_by_closure_set_reverse_lookup() {
        // After building closures, the reverse index should resolve
        // each abstract's closure back to that abstract's Symbol. The
        // mandatory ancestor-collapse in `narrow_complement` will use
        // this to render `Bird` instead of `Eagle | Penguin | Sparrow`.
        //
        // Note: closure(Animal) MUST differ from closure(Bird) here,
        // otherwise the alpha-tie-break would assign closure(Bird)'s
        // slot to `Animal` (which sorts earlier). A non-Bird Animal
        // sibling (`Fish`) breaks the equivalence so the reverse-
        // index entry uniquely identifies `Bird`.
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "abstract type Animal {}\n\
             abstract type Bird extends Animal {}\n\
             type Eagle extends Bird {}\n\
             type Penguin extends Bird {}\n\
             type Sparrow extends Bird {}\n\
             type Fish extends Animal {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let main_mod = pa.index.symbols.intern("main");
        let bird_id = ItemKey::new(main_mod, sym(&pa, "Bird"));
        let bird_closure = pa
            .index
            .subtype_closure
            .get(&bird_id)
            .expect("Bird closure present");
        let resolved = pa
            .index
            .abstract_by_closure_set
            .get(bird_closure.as_ref())
            .copied()
            .expect("reverse lookup hits");
        assert_eq!(resolved, bird_id);
    }

    #[test]
    fn subtype_closure_order_independent_across_source_orderings() {
        // Set-equality is the load-bearing primitive: declaring
        // subtypes in different source orders MUST produce a closure
        // set with the same *contents*, and the reverse-index lookup
        // MUST hit in both. Within a project the canonical form is
        // byte-stable (we sort by `Symbol::Ord`); across projects the
        // resolved-name *order* may differ because each SymbolTable
        // interns in its own order — comparing as sorted name SETS
        // (alphabetical) is what proves the invariant.
        let src_a = "abstract type Bird {}\n\
                     type Eagle extends Bird {}\n\
                     type Penguin extends Bird {}\n\
                     type Sparrow extends Bird {}\n";
        let src_b = "abstract type Bird {}\n\
                     type Sparrow extends Bird {}\n\
                     type Eagle extends Bird {}\n\
                     type Penguin extends Bird {}\n";

        let mut mgr_a = SourceManager::new();
        mgr_a.add_simple(uri("/proj/main.gcl"), src_a, "project", false);
        let pa_a = ProjectAnalysis::analyze(&mgr_a);

        let mut mgr_b = SourceManager::new();
        mgr_b.add_simple(uri("/proj/main.gcl"), src_b, "project", false);
        let pa_b = ProjectAnalysis::analyze(&mgr_b);

        let collect_sorted = |pa: &ProjectAnalysis, name: &str| -> Vec<String> {
            let s = sym(pa, name);
            let main_mod = pa.index.symbols.intern("main");
            let id = ItemKey::new(main_mod, s);
            let mut names: Vec<String> = pa
                .index
                .subtype_closure
                .get(&id)
                .unwrap()
                .iter()
                .map(|id| pa.index.symbols[id.name].to_string())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            collect_sorted(&pa_a, "Bird"),
            collect_sorted(&pa_b, "Bird"),
            "closure must have the same contents regardless of source order"
        );

        // The reverse index hits in both projects via each project's
        // own (canonical) closure key — the within-project byte
        // stability is what matters at narrow-time.
        let bird_id_a = ItemKey::new(pa_a.index.symbols.intern("main"), sym(&pa_a, "Bird"));
        let bird_id_b = ItemKey::new(pa_b.index.symbols.intern("main"), sym(&pa_b, "Bird"));
        assert!(
            pa_a.index
                .abstract_by_closure_set
                .contains_key(pa_a.index.subtype_closure.get(&bird_id_a).unwrap()),
            "reverse index must resolve Bird's closure (src_a)",
        );
        assert!(
            pa_b.index
                .abstract_by_closure_set
                .contains_key(pa_b.index.subtype_closure.get(&bird_id_b).unwrap()),
            "reverse index must resolve Bird's closure (src_b)",
        );
    }

    #[test]
    fn abstract_by_closure_set_tie_break_is_symbol_alpha() {
        // Two abstracts with identical closures — the reverse index
        // keeps the alphabetically-earlier name (deterministic across
        // re-lowers; no dependency on HIR Idx ordering).
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "abstract type Felidae {}\n\
             abstract type CatLike extends Felidae {}\n\
             type Cat extends CatLike {}\n\
             type Lynx extends CatLike {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        // closure(Felidae) == closure(CatLike) == {Cat, Lynx} —
        // `CatLike` sorts alphabetically before `Felidae`, so it wins
        // the reverse-index slot.
        let main_mod = pa.index.symbols.intern("main");
        let catlike_id = ItemKey::new(main_mod, sym(&pa, "CatLike"));
        let cl = pa.index.subtype_closure.get(&catlike_id).unwrap();
        let winner = pa
            .index
            .abstract_by_closure_set
            .get(cl.as_ref())
            .copied()
            .expect("reverse lookup hits");
        let winner_name = &pa.index.symbols[winner.name];
        assert_eq!(
            winner_name, "CatLike",
            "tie-break must pick alphabetically-earlier abstract"
        );
    }

    #[test]
    fn is_abstract_set_captures_abstract_modifier() {
        // Centralized `is_abstract` set lets the narrowing pass ask
        // "is this type closed?" without chasing the owning module's
        // HIR. Captured at ingest time directly from the modifier.
        let mut mgr = SourceManager::new();
        mgr.add_simple(
            uri("/proj/main.gcl"),
            "abstract type Animal {}\n\
             type Cat {}\n\
             abstract type Bird extends Animal {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let main_mod = pa.index.symbols.intern("main");
        let id = |n: &str| ItemKey::new(main_mod, sym(&pa, n));
        assert!(pa.index.is_abstract.contains(&id("Animal")));
        assert!(pa.index.is_abstract.contains(&id("Bird")));
        assert!(!pa.index.is_abstract.contains(&id("Cat")));
    }

    /// `DeclRegistry::record` is idempotent: re-recording the same
    /// `ItemKey` refreshes the cached `Idx<Decl>` without bloating the
    /// map.
    #[test]
    fn decl_registry_record_is_idempotent() {
        use greycat_analyzer_core::SymbolTable;
        let mut r = DeclRegistry::new();
        let decl = Idx::<Decl>::from_raw(0u32);
        let symbols = SymbolTable::new();
        let item = ItemKey::new(symbols.intern("m"), symbols.intern("Foo"));
        r.record(item, decl);
        r.record(item, decl);
        assert_eq!(r.lookup(item), Some(decl));
        assert_eq!(r.len(), 1);
    }
}
