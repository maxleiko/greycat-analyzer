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

use std::hash::{Hash, Hasher};

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
    GenericOwner, ItemId, Primitive, SourceManager, Symbol, SymbolTable, TypeArena, TypeId,
    TypeKind, is_assignable_to, is_castable,
};
use greycat_analyzer_hir::types::{BlockStmt, Decl, Expr, Ident, Stmt, TypeRef};
use greycat_analyzer_hir::{Hir, lower_module};

use crate::analyzer::{
    AnalysisResult, DiagCategory, SemanticDiagnostic, analyze_with_index_into, seed_builtins,
};
use crate::directives::Directives;
use crate::lint::{
    LintDiagnostic, SURFACED_RULES, lint_arrow_on_non_deref_with_directives,
    lint_catch_empty_parens, lint_inferred_return_type_with_directives, lint_no_breakpoint,
    lint_non_exhaustive_with_directives, lint_nullability_with_directives,
    lint_redundant_semicolon, lint_surfaced_with_directives, lint_unreachable_with_directives,
    lint_unused_suppressions, run_lints_with_directives,
};
use crate::resolver::{Resolutions, resolve_with_index_for};
use crate::stdlib::{FnSignature, ProjectIndex};
use crate::well_known::{DeclRegistry, WellKnown};

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

// P14.5
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
// P19
/// The [`TypeArena`] now lives on the
/// project (not per [`AnalysisResult`]). Every module's analyzer mints
/// into the same arena so cross-module `TypeId`s are directly
/// comparable — no `mint_type_shape`/`read_type_shape` translation
/// needed. Callers that previously wrote `module.analysis.types`
/// should call [`Self::arena`] / [`Self::arena_mut`] instead.
#[derive(Debug, Default)]
pub struct ProjectAnalysis {
    pub index: ProjectIndex,
    // P19
    /// Project-wide type arena. Populated alongside every
    /// module's analyzer pass. Append-only and interned, so duplicate
    /// `seed_builtins` calls per `analyze_with_index_into` are a
    /// no-op.
    pub arena: TypeArena,
    // P35.1
    /// Project-wide registry of resolved `(Uri, Idx<Decl>)` →
    /// [`TypeDeclId`]. Issued during signature lowering; consumed by
    /// the type system to identify decls without going through their
    /// SmolStr name.
    pub decl_registry: DeclRegistry,
    // P35.1
    /// Stable handles for the std/core native types the analyzer
    /// special-cases (node-tag auto-deref, runtime sentinels,
    /// collections). Populated during signature lowering. Slots stay
    /// `None` until the corresponding decl flows through the pipeline
    /// (or forever, when std isn't loaded).
    pub well_known: WellKnown,
    // P23.7
    /// When `true`, lint suppressions (`// gcl-lint-off …`)
    /// are still recorded but never silence emissions. Drives the CLI's
    /// `--no-suppressions` flag.
    pub bypass_suppressions: bool,
    // P37.7
    /// Names of rules the caller has explicitly enabled. Only matters
    /// for rules that ship default-off (`default_enabled = false` in
    /// [`LINT_RULES`]); default-on rules are always active. Drives the
    /// CLI's `--on=<rule>` flag, the entrypoint's `@lint_on("…")`
    /// project pragmas (P40), and any future LSP config equivalent.
    pub enabled_rules: FxHashSet<String>,
    // P40.1
    /// Names of rules disabled project-wide. Populated by the
    /// entrypoint's `@lint_off("…")` pragmas. `enabled_rules` and
    /// `disabled_rules` together describe project-wide policy; when
    /// both name the same rule, `disabled_rules` wins (explicit
    /// silence beats explicit enable — matches the CLI precedent of
    /// `--off=X --on=X` silencing X).
    pub disabled_rules: FxHashSet<String>,
    modules: FxHashMap<Uri, ModuleAnalysis>,
    // P19.6
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

// P19.6
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
    // P19.9
    /// `(type_id, attr_sym, ty)` — attr name stays bare Symbol
    /// (per-type-internal).
    attrs: Vec<(ItemId, Symbol, TypeId)>,
    // P19.9
    /// `(type_id, method_sym, ty)`.
    methods: Vec<(ItemId, Symbol, TypeId)>,
    /// `(type_id, method_sym, FnSignature)` — full signature for
    /// methods that declare their own generic params (`<T, …>`).
    /// Lets cross-module / static / instance / arrow method calls
    /// run the same witness-based generic inference the bare-Ident
    /// path uses today (see [`crate::analyzer::Cx::try_generic_call_inference`]).
    method_sigs: Vec<(ItemId, Symbol, FnSignature)>,
    // P19.9
    /// `(fn_id, signature)`.
    fns: Vec<(ItemId, FnSignature)>,
    // P19.10
    /// `(var_id, ty)`. Top-level `var` declared types.
    /// Lowered alongside the other signatures in
    /// [`lower_module_signatures`] so the analyzer's bare-Ident path
    /// can type a cross-module `Definition::ProjectDecl` pointing at
    /// a var.
    vars: Vec<(ItemId, TypeId)>,
    /// `(type_id, supertype_ty)`. Pre-lowered direct supertype shape
    /// (e.g. `Generic { decl: Base, args: [int] }` for
    /// `Sub extends Base<int>`). Populated alongside the other
    /// signatures so `apply_module_contributions` can write back the
    /// instantiated parent TypeId into `TypeMembers::supertype_ty`
    /// — used by `is_assignable_to_with_index` to walk the chain with
    /// real generic args, not just decl identity.
    supertypes: Vec<(ItemId, TypeId)>,
}

impl ProjectAnalysis {
    pub fn new() -> Self {
        let mut arena = TypeArena::new();
        seed_builtins(&mut arena);
        let index = ProjectIndex::new(&mut arena);
        Self {
            index,
            arena,
            decl_registry: crate::well_known::DeclRegistry::new(),
            well_known: crate::well_known::WellKnown::new(),
            bypass_suppressions: false,
            enabled_rules: FxHashSet::default(),
            disabled_rules: FxHashSet::default(),
            modules: FxHashMap::default(),
            sig_cache: FxHashMap::default(),
        }
    }

    /// Borrow the project-wide type arena — required for any
    /// `TypeId` lookup (`arena.get(id)`, `display(arena, id)`, …).
    pub fn arena(&self) -> &TypeArena {
        &self.arena
    }

    /// Mutable borrow of the project-wide type arena. Capability
    /// handlers should not mint new types; this is reserved for the
    /// orchestrator and the staged-pipeline body walker.
    pub fn arena_mut(&mut self) -> &mut TypeArena {
        &mut self.arena
    }

    /// Borrow the project-wide symbol table
    pub fn symbols(&self) -> &SymbolTable {
        &self.index.symbols
    }

    /// Project-wide decl-handle registry — the canonical
    /// `(Uri, Idx<Decl>) → TypeDeclId` interner. Decl *names* live
    /// here too: capability handlers thread the registry into
    /// [`display_type`] / [`display_fqn`] so decl-keyed types render
    /// as their source name.
    pub fn decl_registry(&self) -> &crate::well_known::DeclRegistry {
        &self.decl_registry
    }

    /// Project-wide well-known std/core decl handles. Capability
    /// handlers that need to dispatch on the std-core `node` /
    /// `Array` / etc. identity (rather than the SmolStr name)
    /// consume this via `WellKnown::is_node_tag(decl)` etc.
    pub fn well_known(&self) -> &crate::well_known::WellKnown {
        &self.well_known
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
    pub fn symbol(&self, sym: &Symbol) -> &str {
        &self.index.symbols[*sym]
    }

    /// Resolve an [`ItemId`] to its declared source name through
    /// `id.name → Symbol → SymbolTable`.
    pub fn decl_name(&self, id: ItemId) -> &str {
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
            TypeKind::Generic { decl, args } if !args.is_empty() => (*decl, args.clone()),
            _ => return None,
        };
        let name_sym = decl_id.name;
        let (foreign_uri, foreign_decl_id) = self
            .index
            .locate_decl_in_ns(name_sym, crate::stdlib::Namespace::Type)
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
        out.rebuild(manager);
        out
    }

    /// Rebuild from scratch over the current `manager` state. Existing
    /// cache entries are dropped.
    ///
    // P20
    /// Routes through [`Self::analyze_staged`], the staged-
    /// pipeline orchestrator that future phases will fill in
    /// stage-by-stage. For now, every stage delegates to the existing
    /// per-module + post-pass machinery — same shape, named seam.
    pub fn rebuild(&mut self, manager: &SourceManager) {
        self.analyze_staged(manager);
    }

    // P20
    /// Staged-pipeline entry point. The 12-stage design from
    /// the plan:
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
    ///
    /// **Status:** the staged interface exists but each stage
    /// delegates to the existing per-module + post-pass machinery.
    /// Stages get filled in incrementally:
    /// - Extract S1-S6 (deletes pass 3 `resolve_cross_module_members`).
    /// - Extract S7-S11 (deletes passes 3.4 / 3.45 / 3.52 +
    ///   the receiver-driven substitution shim).
    /// - Rewrite S12 (deletes passes 3.5 / 3.55 / 3.6 +
    ///   the propagate / fixed-point cascade).
    /// - Wire the Q1-Q5 query DAG so `invalidate(uri)` only
    ///   replays Q5(uri) (and Q4 / Q5(others) when signatures change).
    pub fn analyze_staged(&mut self, manager: &SourceManager) {
        self.reset_state();

        // S1 (lower) — parse + lower every module to HIR. Also primes
        // the `ProjectIndex` with each module's top-level decl table so
        // downstream stages can resolve cross-module names. Single
        // forward pass, no per-module dependency.
        let lowered = self.stage_lower(manager);

        // std::fs::create_dir_all("stage").unwrap();
        // std::fs::write("stage/lower.ron", format!("{lowered:#?}")).unwrap();

        // P40.1 — fold the entrypoint's `@lint_off("…")` / `@lint_on("…")`
        // pragmas into the project-wide policy. CLI flags (`--off` /
        // `--on`) have already been merged into `enabled_rules` /
        // `disabled_rules` before `analyze_staged` ran, so this is
        // additive: pragmas widen the sets, CLI flags can also widen
        // them, conflicts within one source aren't resolved here (P40.3
        // adds `conflicting-lint-pragma`). Per-module pragmas in
        // non-entrypoint modules land on `ModuleAnalysis` further down.
        if let Some(entry_uri) = manager.entrypoint_uri() {
            let entry_pragmas = lowered.iter().find_map(|(uri, _, _, _, _, pragmas)| {
                if uri == entry_uri {
                    Some(pragmas)
                } else {
                    None
                }
            });
            if let Some(pragmas) = entry_pragmas {
                self.disabled_rules.extend(pragmas.off.iter().cloned());
                self.enabled_rules.extend(pragmas.on.iter().cloned());
            }
        }

        // S7-S11 (signatures) — lower every type's attr `TypeRef`s and
        // method return-`TypeRef`s into the shared arena project-wide,
        // populating `ProjectIndex::type_members.{attr_types,
        // method_returns}`. With this index in place the analyzer can
        // type a foreign `recv.attr` / `recv.method()` call inline at
        // body-walk time — no post-pass `mint_type_shape` round-trip.
        self.stage_lower_signatures(&lowered);

        // S2-S6 + S12 (per-module slice) — currently bundled inside
        // `analyze_with_index_into`, which threads name declaration,
        // structure declaration, and body walking in one pass. P23
        // peels S12 off into a project-wide body walker.
        self.stage_per_module_analysis(lowered);

        // S12 (cross-module suffix) — post-passes the per-module
        // analyzer can't run because they need every module's
        // signatures to be settled first. P22-P23 absorbs them into
        // the staged S7-S12 work.
        self.stage_cross_module_post_passes(manager);

        // Post-S12 — qualified-ref bookkeeping for the unused-decl
        // lint. Lives outside the 12 stages because it mutates the
        // project index, not the type table.
        self.stage_compute_qualified_refs(manager);

        // std::fs::write("analysis.ron", format!("{self:#?}")).unwrap();

        // P40.1 — apply project-wide rule policy (`disabled_rules`,
        // sourced from CLI `--off` + the entrypoint's `@lint_off(...)`)
        // at the very end, after every emission site has settled.
        // Earlier filtering would be undone by
        // `stage_compute_qualified_refs`'s `unused-decl` re-emission.
        // P40.5 — module-level pragmas no longer apply locally; they
        // emit `lint-pragma-outside-entrypoint` instead. So this sweep
        // only consults the project-wide set.
        self.apply_rule_policy(None);
    }

    // P40.1 + P40.5
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
        self.arena = TypeArena::new();
        seed_builtins(&mut self.arena);
        self.index = ProjectIndex::new(&mut self.arena);
        // P19.6 — cached `TypeId`s reference the old arena, which
        // we just replaced. Drop the cache so the next
        // `lower_signatures_into` rebuilds against the fresh arena.
        self.sig_cache.clear();
    }

    /// **Stage S1** — parse + lower every module to HIR, ingest into
    /// the project index. Returns the lowered modules in document-order
    /// so [`Self::stage_per_module_analysis`] can move them into the
    /// per-module cache without re-lowering.
    fn stage_lower(
        &mut self,
        manager: &SourceManager,
    ) -> Vec<(
        Uri,
        Hir,
        String,
        Duration,
        Directives,
        crate::pragmas::LintPragmas,
    )> {
        // P27.1 — three phases. The middle one runs through the
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
        // P40.5 — capture the entrypoint URI now so the per-module
        // pragma walker can flip behavior when it's NOT the entrypoint.
        let entrypoint_uri: Option<Uri> = manager.entrypoint_uri().cloned();
        let docs: Vec<(Uri, String, String, Tree, bool)> = manager
            .iter()
            .map(|(uri, cell)| {
                let doc = cell.borrow();
                let is_entry = entrypoint_uri.as_ref() == Some(uri);
                (
                    uri.clone(),
                    doc.text.clone(),
                    doc.lib.clone(),
                    doc.tree.clone(),
                    is_entry,
                )
            })
            .collect();

        // Phase B (parallel on native, serial on wasm): lower each
        // module + parse its directives. No shared mutable state.
        let mut lowered: Vec<(
            Uri,
            Hir,
            String,
            Duration,
            Directives,
            crate::pragmas::LintPragmas,
        )> = crate::parallel::par_map(docs, |(uri, text, lib, tree, is_entry)| {
            let lower_start = Instant::now();
            // P35.1 — pass the real module name (filename minus
            // `.gcl`) so the well-known recognizer can match
            // `(lib, module, name)` triples. Default `"module"`
            // only kicks in for URIs without a recognisable
            // filename, which the recognizer ignores anyway.
            let module_name = crate::stdlib::module_name_from_uri(&uri).unwrap_or("module");
            let hir = lower_module(
                &text,
                &self.index.symbols,
                module_name,
                lib.as_str(),
                tree.root_node(),
            );
            let lower_took = lower_start.elapsed();
            let directives = crate::directives::parse_directives(&text, tree.root_node());
            // P40.1 + P40.5 — walk `@lint_off` / `@lint_on` annotations.
            // Entrypoint: collect rules + validate. Other modules: emit
            // `lint-pragma-outside-entrypoint` and discard the rules.
            let pragmas = crate::pragmas::parse_lint_pragmas(&text, tree.root_node(), is_entry);
            (uri, hir, lib, lower_took, directives, pragmas)
        });

        // Phase C (serial): ingest into the project-wide index. This
        // mutates `self.index.symbols` etc., which is `!Send` on
        // purpose — it owns interner state that's amortised across
        // the whole project. `ingest` also mints decl handles into
        // `self.decl_registry`, records well-known runtime slots,
        // and allocates enum TypeIds into the shared arena —
        // folded in so every decl-registration step happens in one
        // place rather than spread across `stage_lower` +
        // `stage_lower_signatures`.
        //
        // Order matters: `ProjectIndex::ingest` is first-wins for
        // module-name claims (later files with the same stem land in
        // `duplicate_modules`). Sort so (a) the entrypoint always wins
        // its module slot — the user's own `project.gcl` should never
        // be flagged as a duplicate of a vendored `lib/foo/project.gcl`
        // — and (b) remaining ties resolve in deterministic URI order
        // so CI runs and local runs agree on which file gets the
        // `duplicate-module-name` diagnostic.
        lowered.sort_by(|(a_uri, _, _, _, _, _), (b_uri, _, _, _, _, _)| {
            let a_is_entry = entrypoint_uri.as_ref() == Some(a_uri);
            let b_is_entry = entrypoint_uri.as_ref() == Some(b_uri);
            b_is_entry
                .cmp(&a_is_entry)
                .then_with(|| a_uri.as_str().cmp(b_uri.as_str()))
        });
        for (uri, hir, _lib, _lower_took, _directives, _pragmas) in &lowered {
            self.index.ingest(
                uri,
                hir,
                &mut self.arena,
                &mut self.decl_registry,
                &mut self.well_known,
            );
        }
        lowered
    }

    /// **Stages S7-S11** — lower every type's attr `TypeRef`s
    /// and method return-`TypeRef`s into the shared arena
    /// project-wide, then store the resulting `TypeId`s on each
    /// type's [`crate::stdlib::TypeMembers`] entry.
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
    fn stage_lower_signatures(
        &mut self,
        lowered: &[(
            Uri,
            Hir,
            String,
            Duration,
            Directives,
            crate::pragmas::LintPragmas,
        )],
    ) {
        let pairs: Vec<(&Uri, &Hir)> = lowered.iter().map(|(u, h, _, _, _, _)| (u, h)).collect();
        // P35.1 / P36.2 — `decl_registry` + `well_known` slot
        // recording happen during [`ProjectIndex::ingest`] now, so
        // the signature-lowering pass below sees a fully-populated
        // registry from its first call. (Previously this stage owned
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

/// `Display`-implementing wrapper returned by
/// [`ProjectAnalysis::display_type`]. Prefixes `<module>::` whenever
/// the bare decl name is ambiguous within the project (≥2 modules
/// export it). When the name is unique, output matches the
/// registry-aware [`display_type`] byte-for-byte.
pub struct ProjectTypeDisplay<'a> {
    project: &'a ProjectAnalysis,
    id: TypeId,
}

impl std::fmt::Display for ProjectTypeDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_qualified(f, self.project.arena(), &self.project.index, self.id)
    }
}

/// Index-aware [`Display`] wrapper for a [`TypeId`]. Renders the same
/// way as [`ProjectAnalysis::display_type`] — `module::Name` when the
/// bare decl name is ambiguous within the project, bare otherwise — but
/// without requiring a full `ProjectAnalysis`. Lets per-module
/// consumers (the lints invoked from `run_typed_lints_for_module`)
/// emit ambiguity-qualified type names in their diagnostic messages so
/// downstream consumers (the `infer-return-type` quickfix) paste the
/// qualified form back into source.
pub fn display_type_qualified<'a>(
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    id: TypeId,
) -> QualifiedTypeDisplay<'a> {
    QualifiedTypeDisplay { arena, index, id }
}

pub struct QualifiedTypeDisplay<'a> {
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    id: TypeId,
}

impl std::fmt::Display for QualifiedTypeDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_qualified(f, self.arena, self.index, self.id)
    }
}

fn write_type_qualified(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    id: TypeId,
) -> std::fmt::Result {
    let ty = arena.get(id);
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Primitive(p) => f.write_str(p.name())?,
        TypeKind::Type(d) => write_decl_qualified(f, index, *d)?,
        TypeKind::Generic { decl, args } => {
            write_decl_qualified(f, index, *decl)?;
            write_args_qualified(f, arena, index, args)?;
        }
        // A type-ref that didn't resolve flows through as opaque
        // `any?` — print the degraded form so callers see the honest
        // shape (the `?` is added by the nullable postfix below
        // because `arena.unresolved()` builds with nullable: true).
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_qualified(f, arena, index, *p)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_qualified(f, arena, index, *r)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_qualified(f, arena, index, *a)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_qualified(f, arena, index, *inner)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

fn write_args_qualified(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    args: &[TypeId],
) -> std::fmt::Result {
    f.write_str("<")?;
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write_type_qualified(f, arena, index, *a)?;
    }
    f.write_str(">")
}

/// Registry-aware [`Display`] wrapper for a [`TypeId`]. Consults
/// `decl_registry` to recover decl names for `Type(d)` / `Generic{decl,
/// args}` so error messages and lint diagnostics surface the real
/// `Foo` / `Map<int, String>`. No module-qualification logic — use
/// [`ProjectAnalysis::display_type`] when ambiguity disambiguation is
/// needed.
pub fn display_type<'a>(
    arena: &'a TypeArena,
    decl_registry: &'a crate::well_known::DeclRegistry,
    symbols: &'a SymbolTable,
    id: TypeId,
) -> TypeWithDecls<'a> {
    TypeWithDecls {
        arena,
        decl_registry,
        symbols,
        id,
    }
}

/// Sibling of [`display_type`] that qualifies decls *only when bare
/// lookup from `current_uri` wouldn't reach them* — i.e. the bare form
/// is ambiguous, private cross-module, or absent. Used by error
/// messages that name foreign decls (e.g. cross-module
/// `argument-type-mismatch`) so the rendered text is itself a valid
/// reference from the diagnostic's source position. Decls reachable
/// bare from `current_uri` stay bare to avoid `core::Map` noise.
pub fn display_type_for_module<'a>(
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    decl_registry: &'a crate::well_known::DeclRegistry,
    id: TypeId,
    current_uri: Option<&'a Uri>,
) -> TypeForModule<'a> {
    TypeForModule {
        arena,
        index,
        decl_registry,
        id,
        current_uri,
    }
}

pub struct TypeForModule<'a> {
    arena: &'a TypeArena,
    index: &'a ProjectIndex,
    decl_registry: &'a crate::well_known::DeclRegistry,
    id: TypeId,
    /// When `Some(uri)`, qualify any decl that bare lookup from `uri`
    /// wouldn't reach (private cross-module, ambiguous across two
    /// public modules, or shadowed). When `None`, qualification falls
    /// back to the project-wide `locate_decl(...).len() > 1`
    /// ambiguity heuristic — same as [`write_type_qualified`].
    current_uri: Option<&'a Uri>,
}

impl std::fmt::Display for TypeForModule<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_for_module(
            f,
            self.arena,
            self.index,
            self.decl_registry,
            self.id,
            self.current_uri,
        )
    }
}

fn write_type_for_module(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &crate::well_known::DeclRegistry,
    id: TypeId,
    current_uri: Option<&Uri>,
) -> std::fmt::Result {
    use greycat_analyzer_core::TypeKind;
    let ty = arena.get(id);
    let decl_name = |d: ItemId, f: &mut std::fmt::Formatter<'_>| -> std::fmt::Result {
        let name = &index.symbols[d.name];
        // Qualify iff bare-name lookup from `current_uri` wouldn't
        // bind to this exact decl — i.e. the bare form would either
        // miss (None) or bind to a different decl.
        let needs_qual = match resolve_decl_handle_from(index, decl_registry, current_uri, name) {
            Some(found) => found != d,
            None => true,
        };
        if needs_qual {
            f.write_str(&index.symbols[d.module])?;
            f.write_str("::")?;
        }
        f.write_str(name)
    };
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Primitive(p) => f.write_str(p.name())?,
        TypeKind::Type(d) => decl_name(*d, f)?,
        TypeKind::Generic { decl, args } => {
            decl_name(*decl, f)?;
            f.write_str("<")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *a, current_uri)?;
            }
            f.write_str(">")?;
        }
        // See `write_type_qualified`: degrade unresolved type-refs to
        // `any?` in display so error messages don't pretend the name
        // resolved to something it didn't.
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *p, current_uri)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_for_module(f, arena, index, decl_registry, *r, current_uri)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&index.symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_for_module(f, arena, index, decl_registry, *a, current_uri)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_for_module(f, arena, index, decl_registry, *inner, current_uri)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

pub struct TypeWithDecls<'a> {
    arena: &'a TypeArena,
    decl_registry: &'a crate::well_known::DeclRegistry,
    symbols: &'a SymbolTable,
    id: TypeId,
}

impl std::fmt::Display for TypeWithDecls<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_type_with_decls(f, self.arena, self.decl_registry, self.symbols, self.id)
    }
}

fn write_type_with_decls(
    f: &mut std::fmt::Formatter<'_>,
    arena: &TypeArena,
    _decl_registry: &crate::well_known::DeclRegistry,
    symbols: &SymbolTable,
    id: TypeId,
) -> std::fmt::Result {
    use greycat_analyzer_core::TypeKind;
    let ty = arena.get(id);
    let decl_name = |d: ItemId, f: &mut std::fmt::Formatter<'_>| -> std::fmt::Result {
        f.write_str(&symbols[d.name])
    };
    match &ty.kind {
        TypeKind::Null => f.write_str("null")?,
        TypeKind::Any => f.write_str("any")?,
        TypeKind::Never => f.write_str("never")?,
        TypeKind::Primitive(p) => f.write_str(p.name())?,
        TypeKind::Type(d) => decl_name(*d, f)?,
        TypeKind::Generic { decl, args } => {
            decl_name(*decl, f)?;
            f.write_str("<")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *a)?;
            }
            f.write_str(">")?;
        }
        // See `write_type_qualified`: degrade unresolved type-refs to
        // `any?` in display so error messages don't pretend the name
        // resolved to something it didn't.
        TypeKind::Unresolved { .. } => f.write_str("any")?,
        TypeKind::GenericParam { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::Lambda { params, ret } => {
            f.write_str("fn(")?;
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *p)?;
            }
            f.write_str(")")?;
            if let Some(r) = ret {
                f.write_str(": ")?;
                write_type_with_decls(f, arena, _decl_registry, symbols, *r)?;
            }
        }
        TypeKind::Enum { name, .. } => f.write_str(&symbols[*name])?,
        TypeKind::Union { alts } => {
            for (i, a) in alts.iter().enumerate() {
                if i > 0 {
                    f.write_str(" | ")?;
                }
                write_type_with_decls(f, arena, _decl_registry, symbols, *a)?;
            }
            if ty.nullable
                && !alts
                    .iter()
                    .any(|a| matches!(arena.get(*a).kind, TypeKind::Null))
            {
                f.write_str(" | null")?;
            }
        }
        TypeKind::TypeOf(inner) => {
            f.write_str("typeof ")?;
            write_type_with_decls(f, arena, _decl_registry, symbols, *inner)?;
        }
    }
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Union { .. }) {
        f.write_str("?")?;
    }
    Ok(())
}

fn write_decl_qualified(
    f: &mut std::fmt::Formatter<'_>,
    index: &ProjectIndex,
    decl: ItemId,
) -> std::fmt::Result {
    // Two same-named items in different modules → render with the
    // `module::` qualifier; otherwise the bare name is unambiguous.
    if index.locate_decl(decl.name).len() > 1 {
        f.write_str(&index.symbols[decl.module])?;
        f.write_str("::")?;
    }
    f.write_str(&index.symbols[decl.name])
}

// P19.6
/// Fingerprint of the project-wide name set used by
/// [`lower_type_ref_project`].
/// We hash the names that *exist* (sorted, so the answer is order-
/// independent) so cached contributions can be reused only when the
/// flip outcome is identical to last time.
fn project_name_set_hash(index: &ProjectIndex) -> u64 {
    use std::collections::BTreeSet;
    let mut names: BTreeSet<&str> = BTreeSet::new();
    // P19.9 — type_names / natives / values are Symbol-keyed; resolve
    // back to text through the project's symbol table for stable
    // string hashing.
    for sym in &index.type_names {
        names.insert(&index.symbols[*sym]);
    }
    for sym in index.natives.signatures.keys() {
        names.insert(&index.symbols[*sym]);
    }
    for sym in &index.values {
        names.insert(&index.symbols[*sym]);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for n in &names {
        n.hash(&mut hasher);
        // Field separator — defends against
        // `["ab", "c"]` vs `["a", "bc"]` colliding.
        0u8.hash(&mut hasher);
    }
    hasher.finish()
}

// P19.6
/// Fingerprint of every byte
/// [`lower_module_signatures_walk`] would read out of `hir`. Walks
/// each top-level type / fn / enum decl name, generic ident text,
/// every reachable [`TypeRef`] (recursively), and the optional
/// marker on each ref. Body statements / expressions are skipped
/// they don't contribute to the project signature index.
fn module_signature_hash(hir: &Hir) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
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
                // P19.10 — top-level vars contribute their declared
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

fn hash_type_ref(
    hasher: &mut std::collections::hash_map::DefaultHasher,
    hir: &Hir,
    tr: Idx<greycat_analyzer_hir::types::TypeRef>,
) {
    let r = &hir.type_refs[tr];
    hir.idents[r.name].symbol.hash(hasher);
    r.optional.hash(hasher);
    for p in &r.params {
        hash_type_ref(hasher, hir, *p);
    }
    0u8.hash(hasher);
}

// P24
/// Free-function variant of [`ProjectAnalysis::stage_lower_signatures`]
/// that takes the arena + index as separate `&mut` borrows. Lets the
/// `invalidate` path build the `(Uri, &Hir)` slice from references
/// into `self.modules` without colliding with the `&mut self` recv
/// the method form would require.
///
// P19.6
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
    decl_registry: &crate::well_known::DeclRegistry,
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
    // source `extends` TypeRef to the parent's `ItemId`. Deferred to
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
/// [`ItemId`]. Same-module supertypes are looked up directly; bare-
/// name cross-module supertypes filter `decl_locations` for the Type
/// namespace, skipping private decls (matches GreyCat's bare-name
/// visibility rule); qualified `mod::Super` supertypes route through
/// `module_names`. Primitives (`int`, `String`, …) are intentionally
/// dropped — they never form a `TypeMembers` entry to walk to.
#[allow(clippy::mutable_key_type)]
fn link_supertypes(index: &mut ProjectIndex, lowered: &[(&Uri, &Hir)]) {
    use crate::stdlib::Namespace;
    use greycat_analyzer_hir::types::Decl;

    // Two-pass to dodge the `&mut index` + `&index` aliasing the
    // resolution helper would otherwise need: first collect every
    // (self_id, parent_id) pair, then apply.
    let mut links: Vec<(ItemId, ItemId)> = Vec::new();
    for (uri, hir) in lowered {
        let Some(module) = hir.module.as_ref() else {
            continue;
        };
        let Some(stem) = crate::stdlib::module_name_from_uri(uri) else {
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
            // Primitives can never be a user type's supertype — skip
            // so the chain walker's lookup doesn't spend a probe on a
            // name guaranteed to miss.
            if matches!(
                &index.symbols[parent_name],
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
                continue;
            }
            let parent_id = if let Some(last) = parent_ref.qualifier.last() {
                // Qualified `mod::Super` — the qualifier's last
                // segment names the owning module.
                let qual_sym = hir.idents[*last].symbol;
                let Some(qual_uri) = index.module_names.get(&qual_sym) else {
                    continue;
                };
                let Some(qual_stem) = crate::stdlib::module_name_from_uri(qual_uri) else {
                    continue;
                };
                let parent_module = index.symbols.intern(qual_stem);
                ItemId::new(parent_module, parent_name)
            } else {
                // Bare `Super` — same-module wins first, then cross-
                // module via the resolver's name-set (filtered to
                // non-private Type-namespace candidates).
                let local = ItemId::new(module_sym, parent_name);
                if index.type_members.contains_key(&local) {
                    local
                } else {
                    let mut hit: Option<ItemId> = None;
                    for (cand_uri, decl) in index.locate_decl_in_ns(parent_name, Namespace::Type) {
                        if index.is_decl_private(cand_uri, decl) {
                            continue;
                        }
                        let Some(cand_stem) = crate::stdlib::module_name_from_uri(cand_uri) else {
                            continue;
                        };
                        let cand_module = index.symbols.intern(cand_stem);
                        hit = Some(ItemId::new(cand_module, parent_name));
                        break;
                    }
                    let Some(found) = hit else { continue };
                    found
                }
            };
            let self_id = ItemId::new(module_sym, hir.idents[td.name].symbol);
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

    // Snapshot every type ItemId we'll need to compute closures for.
    let all_types: Vec<ItemId> = index.type_members.keys().copied().collect();

    // Step 1: invert supertype to direct-child.
    let mut children: FxHashMap<ItemId, Vec<ItemId>> = FxHashMap::default();
    for (id, members) in &index.type_members {
        if let Some(parent) = members.supertype {
            children.entry(parent).or_default().push(*id);
        }
    }

    // Step 2: memoized closure build. Recursive helper; `memo` is
    // shared across roots so sibling subtrees don't redo work.
    fn build(
        id: ItemId,
        children: &FxHashMap<ItemId, Vec<ItemId>>,
        is_abstract: &FxHashSet<ItemId>,
        memo: &mut FxHashMap<ItemId, Box<[ItemId]>>,
    ) {
        if memo.contains_key(&id) {
            return;
        }
        // Sentinel insert defends against accidental cycles in
        // `supertype` (shouldn't occur, but corrupt fixtures or
        // half-loaded projects could otherwise loop here).
        memo.insert(id, Box::default());
        let mut set: FxHashSet<ItemId> = FxHashSet::default();
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
        let mut sorted: Vec<ItemId> = set.into_iter().collect();
        sorted.sort();
        memo.insert(id, sorted.into_boxed_slice());
    }

    let mut closure: FxHashMap<ItemId, Box<[ItemId]>> = FxHashMap::default();
    for id in &all_types {
        build(*id, &children, &index.is_abstract, &mut closure);
    }

    // Step 3: reverse index, abstracts only, ordered alphabetically
    // by (module-name, item-name) text so collisions resolve
    // deterministically across re-lowers. `Symbol::Ord` is u32-order
    // (intern-timing dependent), so resolve back through the symbol
    // table for stable string-order comparison.
    let mut abstract_ids: Vec<ItemId> = index.is_abstract.iter().copied().collect();
    abstract_ids.sort_by(|a, b| {
        (&index.symbols[a.module], &index.symbols[a.name])
            .cmp(&(&index.symbols[b.module], &index.symbols[b.name]))
    });
    let mut reverse: FxHashMap<Box<[ItemId]>, ItemId> = FxHashMap::default();
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
    // now keyed by `ItemId`, each entry maps 1:1 to its type_members
    // entry — no name-match scan needed.
    let mut resolutions: FxHashMap<ItemId, TypeId> = FxHashMap::default();
    for (type_id, flags) in &index.type_flags {
        let Some(method_name) = flags.deref.as_deref() else {
            continue;
        };
        if method_name.is_empty() {
            continue;
        }
        let Some(method_sym) = index.symbols.lookup(method_name) else {
            continue;
        };
        if let Some(ret) = index.type_method_return_chain(*type_id, method_sym) {
            resolutions.insert(*type_id, ret);
        }
    }
    for (type_id, ret_ty) in resolutions {
        if let Some(tm) = index.type_members.get_mut(&type_id) {
            tm.deref_return_ty = Some(ret_ty);
        }
    }
}

// P19.6
/// Walk a single module's signatures and return the
/// contributions it would write into the project index. Mutates
/// `index.symbols` in passing (every contributed name is interned
/// so cache entries can use `Symbol` keys).
///
// P19.9
/// `generics_in_scope` is keyed by [`Symbol`] now, not
/// `String`; `GenericOwner::{Type,Function}` still carry the source
/// text since they're stored on `TypeKind::GenericParam` in the
/// shared arena. The interner makes the *map* lookup cheap; the
/// owner still needs the text for display.
fn lower_module_signatures(
    arena_mut: &mut TypeArena,
    index: &mut ProjectIndex,
    decl_registry: &crate::well_known::DeclRegistry,
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
        // Private decls go through full signature lowering. With every
        // per-item map now keyed by `ItemId` (chunks 3+4), two same-
        // named decls in different modules — including the common
        // public+private collision shape — no longer fight for a
        // single slot. Same-module typing of private fn calls / member
        // access on private-type values / reads of private vars all
        // need the populated `fn_signatures` / `attr_types` /
        // `method_returns` / `var_types` entries. The cross-module
        // bare-name filter is still applied at resolution time via
        // `private_locations`; sig lowering doesn't need a parallel
        // gate.
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
                    // owner=Sub)] }`. Filter out the trivial primitive
                    // cases (matches the symbol-only ingest filter).
                    let parent_sym = hir.idents[hir.type_refs[super_tr].name].symbol;
                    let parent_text: &str = &index.symbols[parent_sym];
                    let is_primitive_parent = matches!(
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
                    );
                    if !is_primitive_parent {
                        let ty = lower_type_ref_project(
                            hir,
                            super_tr,
                            arena_mut,
                            &*index,
                            decl_registry,
                            &generics_in_scope,
                            Some(uri),
                        );
                        entry.supertypes.push((type_id, ty));
                    }
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
                        arena_mut,
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
                            arena_mut,
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
                                arena_mut,
                                &*index,
                                decl_registry,
                                &generics_in_scope,
                                Some(uri),
                            )
                        } else {
                            arena_mut.any_nullable()
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
                            arena_mut,
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
                    arena_mut,
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
                            arena_mut,
                            &*index,
                            decl_registry,
                            &generics_in_scope,
                            Some(uri),
                        )
                    } else {
                        arena_mut.any_nullable()
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
                    arena_mut,
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

// P19.14
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
    well_known: &crate::well_known::WellKnown,
    _decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    from: TypeId,
    to: TypeId,
) -> bool {
    if is_assignable_to(arena, from, to) {
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

        TypeKind::Union { alts } => alts.into_iter().all(|alt| {
            is_assignable_to_with_index(index, well_known, _decl_registry, arena, alt, to)
        }),

        // P-typeof — type-literal source. Accepts `Type(core::type)`
        // as a widening target so stdlib functions typed `(t: type)`
        // continue to accept arguments now typed as `TypeOf(X)`.
        // Identity TypeOf → TypeOf is handled by the core fast path.
        TypeKind::TypeOf(_) => match b_kind {
            TypeKind::Type(d) => well_known.type_decl == Some(d),
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_assignable_to_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
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
                    is_assignable_to(arena, hop, to)
                })
            }
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_assignable_to_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Node-tag bivariance + node<T> covariance + cross-decl
        // generic supertype walk. Same-decl generics stay invariant
        // (handled by core's same-handle args check).
        TypeKind::Generic { decl: da, args: aa } => match b_kind {
            TypeKind::Generic {
                decl: db,
                args: ref ab,
            } if da == db && aa.len() == ab.len() && well_known.is_node_tag(da) => true,
            TypeKind::Generic {
                decl: db,
                args: ref ab,
            } if da == db
                && aa.len() == 1
                && ab.len() == 1
                && well_known.node_decl.is_some_and(|nd| nd == da) =>
            {
                // node<Sub> -> node<Super> when Sub extends Super.
                // Recurse so a chain like node<DeepSub> ->
                // node<MidSub> -> node<Super> works in one hop.
                let (a0, b0) = (aa[0], ab[0]);
                is_assignable_to_with_index(index, well_known, _decl_registry, arena, a0, b0)
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
                    is_assignable_to(arena, hop, to)
                })
            }
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_assignable_to_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
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
            TypeKind::Type(d) if Some(d) == well_known.function_decl => true,
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_assignable_to_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Primitive / Enum / GenericParam: the wrapper adds no
        // inheritance-aware rules beyond what core already covers.
        // Only the target-Union retry is meaningful (a single alt
        // might match via the wrapper's extensions even when core
        // rejected the whole union).
        TypeKind::Primitive(_) | TypeKind::Enum { .. } | TypeKind::GenericParam { .. } => {
            match b_kind {
                TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                    is_assignable_to_with_index(index, well_known, _decl_registry, arena, from, alt)
                }),
                TypeKind::Null
                | TypeKind::Any
                | TypeKind::Never
                | TypeKind::Unresolved { .. }
                | TypeKind::Primitive(_)
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
    sub_decl: ItemId,
    sub_args: &[TypeId],
    mut on_hop: F,
) -> bool
where
    F: FnMut(&mut TypeArena, TypeId) -> bool,
{
    let mut current_decl = sub_decl;
    let mut current_args: Vec<TypeId> = sub_args.to_vec();
    for _ in 0..32 {
        let Some(members) = index.type_members.get(&current_decl) else {
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
            TypeKind::Generic { decl, args } => {
                current_decl = decl;
                current_args = args.to_vec();
            }
            TypeKind::Type(d) => {
                current_decl = d;
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
/// nodeTime<T>` both succeed. Dispatch via `WellKnown::is_node_tag
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
    well_known: &crate::well_known::WellKnown,
    _decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    from: TypeId,
    to: TypeId,
) -> bool {
    if is_castable(arena, from, to) {
        return true;
    }
    // Clone kinds upfront so the immutable arena borrow ends before
    // any recursive call needs `&mut arena` (the chain walker
    // substitutes, which allocates fresh `Generic` nodes).
    let from_kind = arena.get(from).kind.clone();
    let to_kind = arena.get(to).kind.clone();
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
            .any(|alt| is_castable_with_index(index, well_known, _decl_registry, arena, alt, to)),

        // P-typeof — `as` is dropped at runtime; the analyzer mirrors
        // assignability for cast strictness, so accept the widening
        // `TypeOf(X) → Type(core::type)` for the same reason. Other
        // targets reject (identity goes through the early-return).
        TypeKind::TypeOf(_) => match to_kind {
            TypeKind::Type(d) => well_known.type_decl == Some(d),
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_castable_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
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
            TypeKind::Generic { decl: td, .. } => {
                // Upcast (Sub:Type → Sup<args>): walk Sub's chain
                // looking for a hop whose decl matches Sup; that hop
                // is already in fully concrete form, so use core's
                // `is_castable` per-hop.
                if index.is_subtype_of_decl(fd, td) {
                    walk_substituted_supertype_chain(index, arena, fd, &[], |arena, hop| {
                        is_castable(arena, hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast (Sup → Sub<args>): walk Sub (the more-
                    // specific side) with its concrete args, find a
                    // hop matching Sup. Source is non-generic, so the
                    // hop must equal `from` for the cast to make sense.
                    // Defer to core via `is_castable(hop, from)`.
                    let to_decl_args = match arena.get(to).kind.clone() {
                        TypeKind::Generic { args, .. } => args.to_vec(),
                        _ => return false,
                    };
                    walk_substituted_supertype_chain(
                        index,
                        arena,
                        td,
                        &to_decl_args,
                        |arena, hop| is_castable(arena, hop, from),
                    )
                } else {
                    false
                }
            }
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_castable_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
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
        TypeKind::Generic { decl: fd, args: fa } => match to_kind {
            TypeKind::Type(td) => {
                if index.is_subtype_of_decl(fd, td) {
                    // Upcast (Sub<args>:Generic → Sup:Type): walk
                    // Sub's chain with its args; hop matching Sup
                    // (non-generic) is the destination.
                    walk_substituted_supertype_chain(index, arena, fd, &fa, |arena, hop| {
                        is_castable(arena, hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast: target is non-generic but its decl is
                    // a subtype of source's decl. Walk target's chain
                    // (no args), per-hop check against source.
                    walk_substituted_supertype_chain(index, arena, td, &[], |arena, hop| {
                        is_castable(arena, hop, from)
                    })
                } else {
                    false
                }
            }
            TypeKind::Generic {
                decl: td,
                args: ref ta,
            } => {
                if fd == td {
                    // Same decl: core's `is_castable` already gave its
                    // verdict (false, otherwise the early-return fired).
                    // Wrapper-side relaxation: node-tag bivariance —
                    // mirrors `is_assignable_to_with_index` so the two
                    // layers agree on what's allowed for tag args.
                    fa.len() == ta.len() && well_known.is_node_tag(fd)
                } else if index.is_subtype_of_decl(fd, td) {
                    // Upcast: walk source's chain with source's args.
                    walk_substituted_supertype_chain(index, arena, fd, &fa, |arena, hop| {
                        is_castable(arena, hop, to)
                    })
                } else if index.is_subtype_of_decl(td, fd) {
                    // Downcast: walk target's chain with target's args,
                    // per-hop compare against source. Args are checked
                    // invariantly by core's `is_castable`.
                    let ta_owned = ta.to_vec();
                    walk_substituted_supertype_chain(index, arena, td, &ta_owned, |arena, hop| {
                        is_castable(arena, hop, from)
                    })
                } else {
                    false
                }
            }
            TypeKind::Primitive(Primitive::Int) if well_known.is_node_tag(fd) => true,
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_castable_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Source Primitive(Int): inverse `int as <node-tag>` rule.
        // Other primitives have no wrapper-side rules — core handled
        // them already (and returned false here, otherwise the
        // early-return would have fired).
        TypeKind::Primitive(Primitive::Int) => match to_kind {
            TypeKind::Generic { decl: td, .. } if well_known.is_node_tag(td) => true,
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_castable_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },

        // Other source kinds: wrapper adds no rules beyond core; only
        // the target-Union retry might matter (an alt could pick up
        // a wrapper rule even when the whole union didn't).
        TypeKind::Primitive(_)
        | TypeKind::Lambda { .. }
        | TypeKind::Enum { .. }
        | TypeKind::GenericParam { .. } => match to_kind {
            TypeKind::Union { alts } => alts.into_iter().any(|alt| {
                is_castable_with_index(index, well_known, _decl_registry, arena, from, alt)
            }),
            TypeKind::Null
            | TypeKind::Any
            | TypeKind::Never
            | TypeKind::Unresolved { .. }
            | TypeKind::Primitive(_)
            | TypeKind::Type(_)
            | TypeKind::Generic { .. }
            | TypeKind::Lambda { .. }
            | TypeKind::Enum { .. }
            | TypeKind::GenericParam { .. }
            | TypeKind::TypeOf(_) => false,
        },
    }
}

// P19.6
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
    fn stage_per_module_analysis(
        &mut self,
        hirs: Vec<(
            Uri,
            Hir,
            String,
            Duration,
            Directives,
            crate::pragmas::LintPragmas,
        )>,
    ) {
        let bypass = self.bypass_suppressions;
        let index = &self.index;
        // P35.x — the std-core `Map` identity lets the resolver treat
        // `Map { k: v }` keys as value expressions. `Copy`, so the
        // parallel pass-A closure captures it by value.
        let map_decl = self.well_known.map_decl;

        // P26.4 — split the per-module pass into two phases:
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

        let pass_a_run = |(uri, hir, lib, lower_took, mut directives, mut pragmas): (
            Uri,
            Hir,
            String,
            Duration,
            Directives,
            crate::pragmas::LintPragmas,
        )|
         -> PassAOut {
            let t0 = Instant::now();
            let resolutions = resolve_with_index_for(&hir, index, &uri, map_decl);
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

        // P27.1 — single call site, the rayon-vs-serial branch lives
        // in `crate::parallel`.
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
                &self.well_known,
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

    /// **Stage S12 cross-module suffix.** Remaining post-passes:
    ///
    /// - Pass 3.4: cross-module member-expr typing.
    /// - Pass 3.5: cross-module call return-type
    ///   inference for Static / QualifiedStatic / Member / Arrow /
    ///   Ident callees.
    /// - Pass 3.52: re-bind for-in iteration vars
    ///   from now-settled iterable types.
    /// - Fixed-point cascade-closure loop: each pass propagates
    ///   type information one hop; bound at 5 iterations so a
    ///   degenerate/cyclic case can't hang.
    /// - Pass 3.55: typed lint — `arrow-on-non-deref`.
    /// - Pass 3.6: type-relation validation.
    ///
    /// The work each pass does is
    /// subsumed by the staged S7-S11 (which lowers TypeRefs into the
    /// shared arena against the *complete* project name set, so
    /// cross-module attr / method types are resolved at signature
    /// time) and S12 (which walks bodies against fully-resolved
    /// signatures, so call-site monomorphization and member typing
    /// happen inline rather than in a fix-up sweep).
    fn stage_cross_module_post_passes(&mut self, manager: &SourceManager) {
        // P23 — passes 3.4 / 3.45 / 3.5 / 3.52 are all gone. The
        // analyzer's body walker now types Member / Arrow / Static /
        // QualifiedStatic / Ident calls inline via the S7-S11
        // signatures index (`Cx::try_member_call_typing`,
        // `Cx::foreign_member_type`). For-in iterables that depend on
        // those typings settle naturally during the same body walk.
        // Only the typed-lint pass (3.55) and type-relation
        // validation (3.6) survive — both still need to walk every
        // module's now-settled `expr_types`.
        self.run_typed_lints(manager, None);
        self.validate_type_relations(None);
    }

    /// **Post-S12** — bump `references_to` for every decl that's
    /// referenced from another module via a qualified-name access
    /// (`<module>::<name>`, `<module>::<type>::<name>`, etc.). Lets
    /// the unused-decl lint correctly skip `private` decls referenced
    /// through their fully-qualified name from elsewhere.
    fn stage_compute_qualified_refs(&mut self, manager: &SourceManager) {
        self.compute_qualified_refs(manager);
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
        let well_known = &self.well_known;
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
                well_known,
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
                well_known,
                decl_registry,
                cur_uri,
                arena_mut,
                &mut diags,
            );
            collect_object_field_diags_split(
                &self.modules,
                index,
                well_known,
                decl_registry,
                cur_uri,
                arena_mut,
                &mut diags,
            );
            collect_instance_method_value_ref_diags(&self.modules, cur_uri, &mut diags);
            collect_static_type_args_diags(&self.modules, cur_uri, &mut diags);
            collect_object_construction_diags(
                &self.modules,
                arena_mut,
                index,
                well_known,
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
            crate::lint::LintRule::check(&crate::lint::UnusedDecl, &mut cx);
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
        // P24 — pragmatic incremental invalidation. The full Q1-Q5
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
        let mut changed_pragmas: Option<crate::pragmas::LintPragmas> = None;
        let changed_hir = manager.get(uri).map(|cell| {
            let doc = cell.borrow();
            let start = Instant::now();
            // P35.1 — module name from URI for the well-known recogniser.
            let module_name = crate::stdlib::module_name_from_uri(uri).unwrap_or("module");
            let hir = lower_module(
                &doc.text,
                &self.index.symbols,
                module_name,
                &doc.lib,
                doc.root_node(),
            );
            lower_took = start.elapsed();
            changed_lib = Some(doc.lib.clone());
            changed_directives = Some(crate::directives::parse_directives(
                &doc.text,
                doc.root_node(),
            ));
            // P40.1 + P40.5 — re-parse the module's `@lint_off` /
            // `@lint_on` pragmas on every invalidate. Pass `is_entrypoint`
            // so the walker emits `lint-pragma-outside-entrypoint` when
            // pragmas show up in non-entrypoint modules.
            let is_entry = manager.entrypoint_uri() == Some(uri);
            changed_pragmas = Some(crate::pragmas::parse_lint_pragmas(
                &doc.text,
                doc.root_node(),
                is_entry,
            ));
            hir
        });

        // Rebuild `ProjectIndex` from scratch — `ingest` is additive
        // (no removal), so starting empty is what makes the changed
        // doc's deletions visible. **P19.9** — preserve the
        // [`SymbolTable`] so previously-issued [`Symbol`]s (e.g.
        // inside `sig_cache`) remain valid; only the per-module
        // index data gets wiped.
        let preserved_symbols = std::mem::take(&mut self.index.symbols);
        let mut new_index = ProjectIndex::with_symbols(preserved_symbols, &mut self.arena);
        if let Some(hir) = &changed_hir {
            new_index.ingest(
                uri,
                hir,
                &mut self.arena,
                &mut self.decl_registry,
                &mut self.well_known,
            );
        }
        for (other_uri, ma) in &self.modules {
            if other_uri == uri {
                continue;
            }
            new_index.ingest(
                other_uri,
                &ma.hir,
                &mut self.arena,
                &mut self.decl_registry,
                &mut self.well_known,
            );
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
            // P35.1 — module name from URI for the well-known recogniser.
            let module_name = crate::stdlib::module_name_from_uri(other_uri).unwrap_or("module");
            let hir = lower_module(
                &doc.text,
                &self.index.symbols,
                module_name,
                &doc.lib,
                doc.root_node(),
            );
            new_index.ingest(
                other_uri,
                &hir,
                &mut self.arena,
                &mut self.decl_registry,
                &mut self.well_known,
            );
            other_lowered.push((other_uri.clone(), hir, doc.lib.clone(), Duration::ZERO));
        }
        self.index = new_index;

        let Some(hir) = changed_hir else {
            self.modules.remove(uri);
            return;
        };

        // P24 — feed every cached + freshly-lowered HIR through
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
        let resolutions = resolve_with_index_for(&hir, &self.index, uri, self.well_known.map_decl);
        timings.resolve = t0.elapsed();
        let t1 = Instant::now();
        let analysis = analyze_with_index_into(
            &hir,
            &resolutions,
            &self.index,
            &self.well_known,
            &self.decl_registry,
            uri,
            &mut self.arena,
        );
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
    well_known: &crate::well_known::WellKnown,
    decl_registry: &crate::well_known::DeclRegistry,
    arena: &mut TypeArena,
    call: &greycat_analyzer_hir::types::CallExpr,
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
        if !is_assignable_to_with_index(
            index,
            well_known,
            decl_registry,
            arena,
            arg_ty,
            declared_ty,
        ) {
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
    diags: &mut Vec<crate::analyzer::SemanticDiagnostic>,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &crate::well_known::DeclRegistry,
    cur_uri: &Uri,
    runtime_ty: TypeId,
    slot_desc: String,
    byte_range: std::ops::Range<usize>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
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

#[allow(clippy::mutable_key_type)]
fn collect_call_arg_diags_split(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    well_known: &crate::well_known::WellKnown,
    decl_registry: &crate::well_known::DeclRegistry,
    cur_uri: &Uri,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::types::Expr;

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
                well_known,
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
            if !is_assignable_to_with_index(
                index,
                well_known,
                decl_registry,
                arena,
                arg_ty,
                declared_ty,
            ) {
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
                    well_known,
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
    well_known: &WellKnown,
    decl_registry: &DeclRegistry,
    cur_uri: &Uri,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::types::{Expr, ObjectExpr};

    let cur_module = match modules.get(cur_uri) {
        Some(m) => m,
        None => {
            return;
        }
    };
    for (obj_expr_id, expr) in cur_module.hir.exprs.iter() {
        let Expr::Object(ObjectExpr {
            ty: Some(tr_id),
            fields,
            ..
        }) = expr
        else {
            continue;
        };
        let tr = &cur_module.hir.type_refs[*tr_id];
        if !tr.qualifier.is_empty() {
            continue;
        }
        let head_sym = cur_module.hir.idents[tr.name].symbol;
        let Some(head_id) = index.item_id_for(cur_uri, head_sym) else {
            continue;
        };
        let Some(head_members) = index.type_members.get(&head_id) else {
            continue;
        };
        // Positional construction is a separate HIR variant
        // (`Expr::PositionalObject`) and never reaches this named-only
        // validator.
        // Build substitution from the object expr's own settled
        // TypeId. Non-generic head ⇒ empty subst (a no-op). Arity
        // mismatch ⇒ skip the whole expr; the head's lowering pass
        // has already flagged it elsewhere and substituting with
        // the wrong shape would surface noise.
        let Some(obj_ty) = cur_module.analysis.expr_types.get(&obj_expr_id).copied() else {
            continue;
        };
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
        let mut cur_decl = head_id;
        let mut cur_subst = init_subst;
        let mut seen: FxHashSet<ItemId> = FxHashSet::default();
        for _ in 0..32 {
            if !seen.insert(cur_decl) {
                break;
            }
            let Some(m) = index.type_members.get(&cur_decl) else {
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
                TypeKind::Generic { decl, args } => {
                    let Some(parent_m) = index.type_members.get(&decl) else {
                        break;
                    };
                    if parent_m.generics.len() != args.len() {
                        break;
                    }
                    cur_decl = decl;
                    cur_subst = parent_m
                        .generics
                        .iter()
                        .copied()
                        .zip(args.iter().copied())
                        .collect();
                }
                TypeKind::Type(decl) => {
                    cur_decl = decl;
                    cur_subst.clear();
                }
                _ => break,
            }
        }
        for f in fields.iter() {
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
            if !is_assignable_to_with_index(
                index,
                well_known,
                decl_registry,
                arena,
                value_ty,
                declared_ty,
            ) {
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
                    well_known,
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
    use greycat_analyzer_hir::types::Expr;
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
    use greycat_analyzer_hir::types::Expr;

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
/// runtime. The check runs here because it needs `WellKnown`
/// identities for `node` and `Array`, which `parse_diagnostics`
/// doesn't see.
#[allow(clippy::mutable_key_type)]
fn collect_object_construction_diags(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    arena: &TypeArena,
    index: &ProjectIndex,
    well_known: &WellKnown,
    cur_uri: &Uri,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::Severity;
    use greycat_analyzer_hir::types::{Expr, PositionalObjectExpr};

    let Some(cur_module) = modules.get(cur_uri) else {
        return;
    };
    for (obj_expr_id, expr) in cur_module.hir.exprs.iter() {
        // Only the positional form (`Foo { a, b }`) is this pass's
        // concern — named construction (`Foo { k: v }`, including
        // `Map`) is a different HIR variant and handled elsewhere.
        let Expr::PositionalObject(PositionalObjectExpr {
            ty: Some(tr_id),
            fields,
            byte_range,
        }) = expr
        else {
            continue;
        };
        // Empty `T {}` is always a valid default-init.
        if fields.is_empty() {
            continue;
        }
        // Dispatch on the already-settled outer type identity.
        let Some(obj_ty) = cur_module.analysis.expr_types.get(&obj_expr_id).copied() else {
            continue;
        };
        let head_decl = match &arena.get(obj_ty).kind {
            TypeKind::Generic { decl, .. } => *decl,
            TypeKind::Type(decl) => *decl,
            _ => continue,
        };
        if Some(head_decl) == well_known.array_decl {
            continue;
        }
        if Some(head_decl) == well_known.node_decl {
            if fields.len() > 1 {
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "node-init-arity",
                    message: "`node` accepts at most one positional initializer".to_string(),
                    byte_range: byte_range.clone(),
                    category: DiagCategory::TypeRelation,
                });
            }
            continue;
        }

        // The other node-tag family members accept NO initializer at all
        // — only the empty default-init `T {}` is valid (handled by the
        // empty-fields short-circuit above). Unlike `node` (one element)
        // and `Array` (any arity), any content here is a runtime error,
        // and it's neither positional nor named — so we flag it directly
        // instead of falling through to the "use named form" suggestion.
        if Some(head_decl) == well_known.node_list_decl
            || Some(head_decl) == well_known.node_time_decl
            || Some(head_decl) == well_known.node_geo_decl
            || Some(head_decl) == well_known.node_index_decl
        {
            let tr = &cur_module.hir.type_refs[*tr_id];
            let head_name = &index.symbols[cur_module.hir.idents[tr.name].symbol];
            diags.push(SemanticDiagnostic {
                severity: Severity::Error,
                code: "node-tag-no-init",
                message: format!("`{head_name}` does not accept any initializer"),
                byte_range: byte_range.clone(),
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
        let fixed_tuples: [(Option<ItemId>, usize, &[Primitive], &str); 7] = [
            (well_known.t2_decl, 2, &[Primitive::Int], "t2"),
            (
                well_known.t2f_decl,
                2,
                &[Primitive::Float, Primitive::Int],
                "t2f",
            ),
            (well_known.t3_decl, 3, &[Primitive::Int], "t3"),
            (
                well_known.t3f_decl,
                3,
                &[Primitive::Float, Primitive::Int],
                "t3f",
            ),
            (well_known.t4_decl, 4, &[Primitive::Int], "t4"),
            (
                well_known.t4f_decl,
                4,
                &[Primitive::Float, Primitive::Int],
                "t4f",
            ),
            (well_known.str_decl, 1, &[Primitive::String], "str"),
        ];
        let mut matched_v7 = false;
        for &(slot, arity, accepted, type_name) in &fixed_tuples {
            if slot != Some(head_decl) {
                continue;
            }
            matched_v7 = true;
            if fields.len() != arity {
                diags.push(SemanticDiagnostic {
                    severity: Severity::Error,
                    code: "fixed-tuple-arity",
                    message: format!(
                        "`{type_name}` requires exactly {arity} positional initializer{plural} (got {got})",
                        plural = if arity == 1 { "" } else { "s" },
                        got = fields.len(),
                    ),
                    byte_range: byte_range.clone(),
                    category: DiagCategory::TypeRelation,
                });
            } else {
                for value in fields.iter() {
                    let Some(val_ty) = cur_module.analysis.expr_types.get(value).copied() else {
                        continue;
                    };
                    let ok = matches!(
                        &arena.get(val_ty).kind,
                        TypeKind::Primitive(p) if accepted.contains(p),
                    );
                    if !ok {
                        let accepted_msg = accepted
                            .iter()
                            .map(|p| format!("`{}`", p.name()))
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

        let tr = &cur_module.hir.type_refs[*tr_id];
        let head_name = &index.symbols[cur_module.hir.idents[tr.name].symbol];
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            code: "positional-object-init",
            message: format!(
                "`{head_name}` does not accept positional initializers; use named form `{head_name} {{ field: value }}`"
            ),
            byte_range: byte_range.clone(),
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
// P26.3
/// Per-module typed-lint runner extracted out of [`ProjectAnalysis::run_typed_lints`]
/// so the parallel and serial paths share one body and a future
/// regression doesn't drift between them.
///
/// Reads `arena` + `index` immutably; writes only to `module.lints` /
/// `module.directives`. `doc_data` is consulted for the `catch-empty-parens`
/// lint, which needs the source text + parsed tree (the HIR drops the
/// empty `()` shape).
// P27.1 — the cfg gate is gone; the helper is the canonical body
// for both native (rayon-driven) and wasm (serial fallback) call
// sites in `run_typed_lints`.
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
    // P40.1 + P40.5 — a rule fires if any project-wide opt-in surface
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
        // P37.7 + P40.1 — advisory, default-off. Runs when any opt-in
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

    // P40.1 — final project / module-pragma filter.
    // P40.1 — the `disabled_rules` / `pragma_disabled_rules` filter
    // doesn't live in this function. Subsequent passes
    // (`stage_compute_qualified_refs`) re-emit lints, so the policy
    // filter has to land in one place after every emission settles —
    // see [`ProjectAnalysis::apply_rule_policy`], called at the tail
    // of `analyze_staged` and `invalidate`.
}

/// the analyzer's per-module pass deferred. Reads only — never
/// mutates `module`. The shared project arena is passed in;
/// any newly-needed declared-side TypeIds are minted into it
/// alongside everything else, which is fine because the arena is
/// append-only and intern-collapsed.
fn validate_module_type_relations(
    module: &ModuleAnalysis,
    cur_uri: &Uri,
    index: &ProjectIndex,
    well_known: &WellKnown,
    decl_registry: &DeclRegistry,
    arena: &mut TypeArena,
    diags: &mut Vec<SemanticDiagnostic>,
) {
    use crate::analyzer::{SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::types::Decl;

    let hir = &module.hir;
    let analysis = &module.analysis;
    let bool_t = arena.primitive(Primitive::Bool);

    let Some(top) = hir.module.as_ref() else {
        return;
    };
    for d_id in &top.decls {
        validate_decl(
            hir,
            analysis,
            cur_uri,
            index,
            well_known,
            decl_registry,
            arena,
            bool_t,
            &hir.decls[*d_id],
            diags,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_decl(
        hir: &Hir,
        analysis: &AnalysisResult,
        cur_uri: &Uri,
        index: &ProjectIndex,
        well_known: &WellKnown,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        bool_t: TypeId,
        decl: &Decl,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        match decl {
            Decl::Fn(fnd) => {
                let return_ty = fnd.return_type.and_then(|t| {
                    lower_type_ref_id(
                        hir,
                        t,
                        &analysis.registry,
                        index,
                        decl_registry,
                        &analysis.type_decls,
                        arena,
                        Some(cur_uri),
                    )
                });
                if let Some(body) = fnd.body {
                    validate_stmt(
                        hir,
                        analysis,
                        cur_uri,
                        index,
                        well_known,
                        decl_registry,
                        arena,
                        bool_t,
                        body,
                        return_ty,
                        diags,
                    );
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
                            well_known,
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
                for m in &td.methods {
                    validate_decl(
                        hir,
                        analysis,
                        cur_uri,
                        index,
                        well_known,
                        decl_registry,
                        arena,
                        bool_t,
                        &hir.decls[*m],
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
                        well_known,
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
        well_known: &WellKnown,
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
                well_known,
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
        well_known: &WellKnown,
        decl_registry: &DeclRegistry,
        arena: &mut TypeArena,
        bool_t: TypeId,
        stmt_id: Idx<Stmt>,
        return_ty: Option<TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        use greycat_analyzer_hir::types::{
            AssignStmt, AtStmt, DoWhileStmt, ForInStmt, ForStmt, IfStmt, LocalVar, Stmt, TryStmt,
            WhileStmt,
        };
        match &hir.stmts[stmt_id] {
            Stmt::Block(b) => validate_block(
                hir,
                analysis,
                cur_uri,
                index,
                well_known,
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
                        well_known,
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
                        well_known,
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
                    well_known,
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
                        well_known,
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
                    well_known,
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
                    well_known,
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
                    well_known,
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
                    well_known,
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
                    well_known,
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
                    well_known,
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
                    well_known,
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
                        well_known,
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
                        && is_assignable_to_with_index(
                            index,
                            well_known,
                            decl_registry,
                            arena,
                            value_ty,
                            rt,
                        )
                        && !is_assignable_to_with_index(
                            index,
                            well_known,
                            decl_registry,
                            arena,
                            runtime_ty,
                            rt,
                        )
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
            Stmt::Return(_)
            | Stmt::Expr(_)
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
        well_known: &WellKnown,
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
        if is_assignable_to_with_index(
            index,
            well_known,
            decl_registry,
            arena,
            value_ty,
            declared_ty,
        ) {
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
        decl_registry: &crate::well_known::DeclRegistry,
        expr_id: Idx<Expr>,
        bool_t: TypeId,
        label: &'static str,
        hir: &Hir,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        let Some(ty) = analysis.expr_types.get(&expr_id).copied() else {
            return;
        };
        if is_assignable_to(arena, ty, bool_t) {
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

// P22
/// Project-wide TypeRef lowerer used by
/// [`ProjectAnalysis::stage_lower_signatures`]. Mirrors
/// `Cx::lower_type_ref` but uses the project index instead of a
/// per-module registry, so foreign type names resolve directly to
/// `Named { name }` in the shared arena. `generics_in_scope` maps
/// the names of the generic params owned by the current type / fn to
/// their `GenericOwner`, so `T` lowers to `GenericParam(T, owner=…)`
/// instead of `Named { name: "T" }`.
fn lower_type_ref_project(
    hir: &Hir,
    type_ref: Idx<TypeRef>,
    arena: &mut TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    generics_in_scope: &FxHashMap<Symbol, GenericOwner>,
    current_uri: Option<&Uri>,
) -> TypeId {
    let tr = hir.type_refs[type_ref].clone();
    // Qualified ref (`b::Foo`, …): bypass the bare-name ladder. The
    // resolver-side `bind_qualified_type_leaf` and the body walker's
    // `lower_qualified_type_ref` use the same module-scoped lookup;
    // signature lowering does too so all three agree on the shape.
    if !tr.qualifier.is_empty() {
        return lower_qualified_type_ref_project(
            hir,
            &tr,
            arena,
            index,
            decl_registry,
            generics_in_scope,
            current_uri,
        );
    }
    let name_sym = hir.idents[tr.name].symbol;
    let name = &index.symbols[name_sym];
    // Same-module-first decl handle lookup: `private` decls declared
    // in `current_uri` resolve to themselves; cross-module bare-name
    // lookups still filter out foreign private decls (matches
    // GreyCat's `private` semantics enforced in the resolver's
    // `record_use`).
    let lookup = |name: &str| resolve_decl_handle_from(index, decl_registry, current_uri, name);
    let mut base = match name {
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
            if !tr.params.is_empty() {
                let args: Vec<TypeId> = tr
                    .params
                    .iter()
                    .map(|p| {
                        lower_type_ref_project(
                            hir,
                            *p,
                            arena,
                            index,
                            decl_registry,
                            generics_in_scope,
                            current_uri,
                        )
                    })
                    .collect();
                match lookup(name) {
                    Some(handle) => arena.generic(handle, args),
                    None => arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end)),
                }
            } else if let Some(owner) = generics_in_scope.get(&name_sym) {
                arena.generic_param(name_sym, *owner)
            } else if let Some(arity) = lookup(name)
                .and_then(|item| index.type_members.get(&item))
                .map(|m| m.generics.len())
                .filter(|n| *n > 0)
            {
                // Raw-form generic reference: `Tensor` (no params)
                // ≡ `Tensor<any?, any?>`. Expand at lowering time so
                // the body walker and validation pass agree on the
                // same shape; kills the need for any raw-form bridge
                // in `is_assignable_to`. Routes through
                // `resolve_decl_handle_from` so the ItemId picks the
                // same non-private candidate as everywhere else, with
                // same-module private preferred when applicable.
                let any_q = arena.any_nullable();
                let args: Vec<TypeId> = vec![any_q; arity];
                match lookup(name) {
                    Some(handle) => arena.generic(handle, args),
                    None => arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end)),
                }
            } else if let Some(enum_id) =
                lookup(name).and_then(|item| index.enum_types.get(&item).copied())
            {
                // P19.10 — canonical enum TypeId from S7-S11.
                // Without this, a cross-module enum reference would
                // mint `Named(name)` (kind != Enum), which breaks
                // the analyzer's `Static` enum-variant arm
                // (`if let TypeKind::Enum { variants, .. } = ...`).
                enum_id
            } else if let Some(handle) = lookup(name) {
                // Non-generic concrete type with a known home decl:
                // mint a handle-keyed `Type(handle)` so it interns
                // equal to whatever `register_module_types` produced
                // for the same decl in the per-module analyzer.
                arena.alloc_type(handle)
            } else {
                // No reachable decl: `Unresolved` so hover / display
                // surface the typo'd name verbatim, behaves like
                // `any?` for assignability.
                arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end))
            }
        }
    };
    if tr.typeof_marker {
        // P-typeof — same wrapping rule as the in-module
        // `Cx::lower_type_ref`. Lifts the lowered inner into a
        // `TypeOf(...)` so cross-module `FnSignature.params` end up
        // with the typeof-aware shape and generic inference can match
        // them against type-literal arguments.
        base = arena.type_of(base);
    }
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}

/// P36.2 — resolve a type name to its handle via the project's decl
/// table and registry. Walks every `(uri, decl)` pair recorded for
/// `name` and returns the first one already interned in
/// `decl_registry`. Returns `None` when the name has no recorded
/// location yet (the per-module analyzer's `register_module_types`
/// then falls back to `arena.unresolved`).
// P38.2 — exposed crate-wide so the analyzer's in-module
// `lower_type_ref` can mint `Type(handle)` for foreign non-generic
// types.
pub fn resolve_decl_handle(
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    name: &str,
) -> Option<ItemId> {
    resolve_decl_handle_from(index, decl_registry, None, name)
}

/// Same as [`resolve_decl_handle`] but takes the *current* module
/// URI so same-module access wins over cross-module candidates and
/// can reach private decls in its own module (matches GreyCat's
/// `private` semantics — bare-name only requires FQN *across*
/// modules, not within).
pub fn resolve_decl_handle_from(
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    current_uri: Option<&Uri>,
    name: &str,
) -> Option<ItemId> {
    let name_sym = index.symbols.lookup(name)?;
    // Type-namespace only: this mints an [`ItemId`], so a
    // same-named `Fn` / `Var` decl must never be returned.
    //
    // Two-pass: same-module candidates first (unfiltered — private
    // is visible from within its own module), then cross-module
    // non-private candidates (private gets filtered to mirror the
    // resolver's `is_decl_private` rule in `record_use`).
    if let Some(cur) = current_uri {
        for (uri, _) in index.locate_decl_in_ns(name_sym, crate::stdlib::Namespace::Type) {
            if uri == cur
                && let Some(item) = index.item_id_for(uri, name_sym)
                && decl_registry.lookup(item).is_some()
            {
                return Some(item);
            }
        }
    }
    for (uri, decl) in index.locate_decl_in_ns(name_sym, crate::stdlib::Namespace::Type) {
        if index.is_decl_private(uri, decl) {
            continue;
        }
        if let Some(item) = index.item_id_for(uri, name_sym)
            && decl_registry.lookup(item).is_some()
        {
            return Some(item);
        }
    }
    None
}

/// Signature-side counterpart of [`crate::analyzer::Cx::lower_qualified_type_ref`].
/// Same module-scoped resolution shape — the three lowering paths
/// (signature, body walker, resolver) must agree on which decl a
/// qualified ref binds to, or cross-arena identity breaks.
fn lower_qualified_type_ref_project(
    hir: &Hir,
    tr: &TypeRef,
    arena: &mut TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    generics_in_scope: &FxHashMap<Symbol, GenericOwner>,
    current_uri: Option<&Uri>,
) -> TypeId {
    let module_segment = *tr
        .qualifier
        .last()
        .expect("lower_qualified_type_ref_project called with empty qualifier");
    let module_name = hir.idents[module_segment].symbol;
    let leaf_name = hir.idents[tr.name].symbol;
    let byte_span = (tr.byte_range.start, tr.byte_range.end);

    let Some(module_uri) = index.module_names.get(&module_name) else {
        let mut base = arena.unresolved(leaf_name, byte_span);
        if tr.typeof_marker {
            base = arena.type_of(base);
        }
        if tr.optional {
            base = arena.nullable(base);
        }
        return base;
    };

    if !tr.params.is_empty() {
        let args: Vec<TypeId> = tr
            .params
            .iter()
            .map(|p| {
                lower_type_ref_project(
                    hir,
                    *p,
                    arena,
                    index,
                    decl_registry,
                    generics_in_scope,
                    current_uri,
                )
            })
            .collect();
        let mut base = match index
            .item_id_for(module_uri, leaf_name)
            .filter(|item| decl_registry.lookup(*item).is_some())
        {
            Some(item) => arena.generic(item, args),
            None => arena.unresolved(leaf_name, byte_span),
        };
        if tr.typeof_marker {
            base = arena.type_of(base);
        }
        if tr.optional {
            base = arena.nullable(base);
        }
        return base;
    }

    let mut base = match index
        .item_id_for(module_uri, leaf_name)
        .filter(|item| decl_registry.lookup(*item).is_some())
    {
        Some(item) => arena.alloc_type(item),
        None => arena.unresolved(leaf_name, byte_span),
    };
    if tr.typeof_marker {
        base = arena.type_of(base);
    }
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}

/// Look up a syntactic `TypeRef` and mint a corresponding `TypeId`
/// into `arena`. `arena` is the validation-pass's working clone of
/// `analysis.types`, so any new mints land where `is_assignable_to`
/// can see them.
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
    let tr = &hir.type_refs[type_ref];
    // Qualified ref (`b::Foo`, …) — route through the same
    // module-scoped lookup the resolver and the body walker use, so
    // all four lowering paths agree on the leaf decl identity.
    if !tr.qualifier.is_empty() {
        let generics_in_scope = FxHashMap::default();
        return Some(lower_qualified_type_ref_project(
            hir,
            tr,
            arena,
            index,
            decl_registry,
            &generics_in_scope,
            current_uri,
        ));
    }
    let name_sym = hir.idents[tr.name].symbol;
    let base = match &index.symbols[name_sym] {
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
            let name = &index.symbols[name_sym];
            // Same-module-first decl handle lookup — mirrors the
            // `lookup` closure in `lower_type_ref_project`. Reaches
            // `private` decls in `current_uri`'s own module.
            let lookup =
                |name: &str| resolve_decl_handle_from(index, decl_registry, current_uri, name);
            if !tr.params.is_empty() {
                let mut args = Vec::with_capacity(tr.params.len());
                for p in &tr.params {
                    args.push(lower_type_ref_id(
                        hir,
                        *p,
                        registry,
                        index,
                        decl_registry,
                        type_decls,
                        arena,
                        current_uri,
                    )?);
                }
                match lookup(name) {
                    Some(handle) => arena.generic(handle, args),
                    None => arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end)),
                }
            } else if let Some(arity) = generic_arity_for(name_sym, hir, type_decls, index) {
                // Raw-form generic reference: `Tensor` (no params)
                // ≡ `Tensor<any?, any?>`. Expand here so the
                // validation pass and the body walker's
                // `lower_type_ref` produce the same shape.
                let any_q = arena.any_nullable();
                let args: Vec<TypeId> = vec![any_q; arity];
                match lookup(name) {
                    Some(handle) => arena.generic(handle, args),
                    None => arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end)),
                }
            } else if let Some(id) = registry.lookup(name_sym) {
                id
            } else if let Some(enum_id) =
                lookup(name).and_then(|item| index.enum_types.get(&item).copied())
            {
                // Canonical enum TypeId from S7-S11. The body walker's
                // `lower_type_ref` and `lower_type_ref_project` both
                // hit this branch ahead of `resolve_decl_handle`; the
                // validation pass has to agree or `Generic("Array",
                // [Enum{...}])` (body) vs `Generic("Array",
                // [Type(handle)])` (validation) fails invariant
                // arg-equality.
                enum_id
            } else if let Some(handle) = lookup(name) {
                // Foreign non-generic decl: mint `Type(handle)` to
                // match what the body walker's `lower_type_ref` and
                // the signature pass's `lower_type_ref_project`
                // produce. Without this, the validation pass would
                // mint a different shape than the body walker for
                // the same source token — surfacing as a self-named
                // "T is not assignable to T" diagnostic.
                arena.alloc_type(handle)
            } else {
                arena.unresolved(name_sym, (tr.byte_range.start, tr.byte_range.end))
            }
        }
    };
    let mut base = base;
    if tr.typeof_marker {
        base = arena.type_of(base);
    }
    Some(if tr.optional {
        arena.nullable(base)
    } else {
        base
    })
}

/// Shared arity lookup used by `lower_type_ref_id` and (via
/// `Cx::generic_arity_for` in `analyzer.rs`) by `lower_type_ref`.
/// Resolution order: a local declaration shadows the project-wide
/// index. Returns `None` for non-generic decls (arity 0) so the
/// caller can fall through to the non-generic branches.
fn generic_arity_for(
    name_sym: Symbol,
    hir: &greycat_analyzer_hir::Hir,
    type_decls: &rustc_hash::FxHashMap<Symbol, Idx<Decl>>,
    index: &ProjectIndex,
) -> Option<usize> {
    if let Some(decl_id) = type_decls.get(&name_sym)
        && let Decl::Type(td) = &hir.decls[*decl_id]
        && !td.generics.is_empty()
    {
        return Some(td.generics.len());
    }
    // Bare-name foreign fallback: walk every non-private Type-ns
    // candidate (mirrors `resolve_decl_handle`'s shape); first with
    // non-zero arity wins.
    for (uri, decl) in index.locate_decl_in_ns(name_sym, crate::stdlib::Namespace::Type) {
        if index.is_decl_private(uri, decl) {
            continue;
        }
        let Some(item) = index.item_id_for(uri, name_sym) else {
            continue;
        };
        let arity = index.type_members.get(&item)?.generics.len();
        if arity > 0 {
            return Some(arity);
        }
    }
    None
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
    callee_expr: &greycat_analyzer_hir::types::Expr,
) -> (GenericsInScope, MethodSubst) {
    use greycat_analyzer_hir::types::Expr;
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
        TypeKind::Generic { decl, args } => (&index.symbols[decl.name], args.as_slice()),
        _ => return empty(),
    };
    let Some(module) = fn_module.hir.module.as_ref() else {
        return empty();
    };
    let mut owner_td: Option<&greycat_analyzer_hir::types::TypeDecl> = None;
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
    /// the lookup is a direct ItemId construction.
    fn closure_names<'a>(pa: &'a ProjectAnalysis, root: &str) -> Vec<&'a str> {
        let root_sym = sym(pa, root);
        let main_mod = pa.index.symbols.intern("main");
        let root_id = ItemId::new(main_mod, root_sym);
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
        let bird_id = ItemId::new(main_mod, sym(&pa, "Bird"));
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
            let id = ItemId::new(main_mod, s);
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
        let bird_id_a = ItemId::new(pa_a.index.symbols.intern("main"), sym(&pa_a, "Bird"));
        let bird_id_b = ItemId::new(pa_b.index.symbols.intern("main"), sym(&pa_b, "Bird"));
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
        let catlike_id = ItemId::new(main_mod, sym(&pa, "CatLike"));
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
        let id = |n: &str| ItemId::new(main_mod, sym(&pa, n));
        assert!(pa.index.is_abstract.contains(&id("Animal")));
        assert!(pa.index.is_abstract.contains(&id("Bird")));
        assert!(!pa.index.is_abstract.contains(&id("Cat")));
    }
}
