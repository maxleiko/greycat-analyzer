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
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_hir::{Hir, lower_module};

use crate::analyzer::{AnalysisResult, analyze_with_index_into, seed_builtins};
use crate::directives::Directives;
use crate::lint::{
    LintDiagnostic, lint_arrow_on_non_deref_with_directives, lint_catch_empty_parens,
    lint_inferred_return_type_with_directives, lint_non_exhaustive_with_directives,
    lint_nullability_with_directives, lint_redundant_semicolon, lint_unreachable_with_directives,
    lint_unused_suppressions, run_lints_with_directives,
};
use crate::resolver::{Resolutions, resolve_with_index};
use crate::stdlib::{FnSignature, ProjectIndex};

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
/// The [`greycat_analyzer_types::TypeArena`] now lives on the
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
    pub arena: greycat_analyzer_types::TypeArena,
    // P35.1
    /// Project-wide registry of resolved `(Uri, Idx<Decl>)` →
    /// [`TypeDeclId`]. Issued during signature lowering; consumed by
    /// the type system to identify decls without going through their
    /// SmolStr name.
    pub decl_registry: crate::well_known::DeclRegistry,
    // P35.1
    /// Stable handles for the std/core native types the analyzer
    /// special-cases (node-tag auto-deref, runtime sentinels,
    /// collections). Populated during signature lowering. Slots stay
    /// `None` until the corresponding decl flows through the pipeline
    /// (or forever, when std isn't loaded).
    pub well_known: crate::well_known::WellKnown,
    // P23.7
    /// When `true`, lint suppressions (`// gcl-lint-off …`)
    /// are still recorded but never silence emissions. Drives the CLI's
    /// `--no-suppressions` flag.
    pub bypass_suppressions: bool,
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
    /// `(type_sym, attr_sym, ty)`.
    attrs: Vec<(
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::TypeId,
    )>,
    // P19.9
    /// `(type_sym, method_sym, ty)`.
    methods: Vec<(
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::TypeId,
    )>,
    // P19.9
    /// `(fn_sym, signature)`.
    fns: Vec<(greycat_analyzer_types::Symbol, FnSignature)>,
    // P19.9
    /// `(enum_sym, ty)`.
    enums: Vec<(
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::TypeId,
    )>,
    // P19.10
    /// `(var_sym, ty)`. Top-level `var` declared types.
    /// Lowered alongside the other signatures in
    /// [`lower_module_signatures`] so the analyzer's bare-Ident path
    /// can type a cross-module `Definition::ProjectDecl` pointing at
    /// a var.
    vars: Vec<(
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::TypeId,
    )>,
}

impl ProjectAnalysis {
    pub fn new() -> Self {
        let mut arena = greycat_analyzer_types::TypeArena::new();
        seed_builtins(&mut arena);
        Self {
            index: ProjectIndex::new(),
            arena,
            decl_registry: crate::well_known::DeclRegistry::new(),
            well_known: crate::well_known::WellKnown::new(),
            bypass_suppressions: false,
            modules: FxHashMap::default(),
            sig_cache: FxHashMap::default(),
        }
    }

    /// Borrow the project-wide type arena — required for any
    /// `TypeId` lookup (`arena.get(id)`, `display(arena, id)`, …).
    pub fn arena(&self) -> &greycat_analyzer_types::TypeArena {
        &self.arena
    }

    /// Mutable borrow of the project-wide type arena. Capability
    /// handlers should not mint new types; this is reserved for the
    /// orchestrator and the staged-pipeline body walker.
    pub fn arena_mut(&mut self) -> &mut greycat_analyzer_types::TypeArena {
        &mut self.arena
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
    }

    /// Reset every cached field so a `rebuild` / `analyze_staged`
    /// run starts from a known-empty state. Re-seeding builtins is
    /// idempotent (the arena interns them on the second insert).
    fn reset_state(&mut self) {
        self.index = ProjectIndex::new();
        self.modules.clear();
        self.arena = greycat_analyzer_types::TypeArena::new();
        seed_builtins(&mut self.arena);
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
    ) -> Vec<(Uri, Hir, String, Duration, Directives)> {
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
        let docs: Vec<(
            Uri,
            String,
            String,
            greycat_analyzer_syntax::tree_sitter::Tree,
        )> = manager
            .iter()
            .map(|(uri, cell)| {
                let doc = cell.borrow();
                (
                    uri.clone(),
                    doc.text.clone(),
                    doc.lib.clone(),
                    doc.tree.clone(),
                )
            })
            .collect();

        // Phase B (parallel on native, serial on wasm): lower each
        // module + parse its directives. No shared mutable state.
        let lowered: Vec<(Uri, Hir, String, Duration, Directives)> =
            crate::parallel::par_map(docs, |(uri, text, lib, tree)| {
                let lower_start = Instant::now();
                // P35.1 — pass the real module name (filename minus
                // `.gcl`) so the well-known recognizer can match
                // `(lib, module, name)` triples. Default `"module"`
                // only kicks in for URIs without a recognisable
                // filename, which the recognizer ignores anyway.
                let module_name = crate::stdlib::module_name_from_uri(&uri)
                    .unwrap_or_else(|| "module".to_string());
                let hir = lower_module(&text, module_name, lib.as_str(), tree.root_node());
                let lower_took = lower_start.elapsed();
                let directives = crate::directives::parse_directives(&text, tree.root_node());
                (uri, hir, lib, lower_took, directives)
            });

        // Phase C (serial): ingest into the project-wide index. This
        // mutates `self.index.symbols` etc., which is `!Send` on
        // purpose — it owns interner state that's amortised across
        // the whole project.
        for (uri, hir, _lib, _lower_took, _directives) in &lowered {
            self.index.ingest(uri, hir);
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
    fn stage_lower_signatures(&mut self, lowered: &[(Uri, Hir, String, Duration, Directives)]) {
        let pairs: Vec<(&Uri, &Hir)> = lowered.iter().map(|(u, h, _, _, _)| (u, h)).collect();
        lower_signatures_into(
            &mut self.arena,
            &mut self.index,
            &pairs,
            &mut self.sig_cache,
        );
        // P35.1 — populate decl_registry + well_known. Runs after the
        // existing signature lowering so we walk the same `Decl::Type` /
        // `Decl::Enum` set without disturbing the cache hashing above.
        // Cheap: one decl walk per module, idempotent within a single
        // `DeclRegistry`. We don't bother with sig-cache parity because
        // the registry is append-only and slot writes are idempotent on
        // the same `(lib, module, name)` triple.
        for (uri, hir) in &pairs {
            let Some(module) = hir.module.as_ref() else {
                continue;
            };
            for d_id in &module.decls {
                match &hir.decls[*d_id] {
                    Decl::Type(td) => {
                        let name = hir.idents[td.name].text.as_str();
                        let decl_id = self.decl_registry.get_or_insert(uri, *d_id, name);
                        self.well_known
                            .record(&module.lib, &module.name, name, decl_id);
                    }
                    Decl::Enum(ed) => {
                        // Enums get a handle too — needed for the
                        // cross-arena Enum↔Named bridge cleanup in P35.7.
                        let name = hir.idents[ed.name].text.as_str();
                        let _ = self.decl_registry.get_or_insert(uri, *d_id, name);
                    }
                    _ => {}
                }
            }
        }
    }
}

// P19.6
/// Fingerprint of the project-wide name set used by
/// [`lower_type_ref_project`]. `lower_type_ref_project` checks
/// `index.has_name(...)` for every non-primitive, non-generic-param
/// TypeRef name; the answer flips between `Named(name)` and `any()`.
/// We hash the names that *exist* (sorted, so the answer is order-
/// independent) so cached contributions can be reused only when the
/// flip outcome is identical to last time.
fn project_name_set_hash(index: &ProjectIndex) -> u64 {
    use std::collections::BTreeSet;
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for n in index.registry.iter_names() {
        names.insert(n);
    }
    // P19.9 — natives + values are Symbol-keyed; resolve back to text
    // through the project's symbol table for stable string hashing.
    for sym in index.natives.signatures.keys() {
        if let Some(s) = index.symbols.resolve(*sym) {
            names.insert(s);
        }
    }
    for sym in &index.values {
        if let Some(s) = index.symbols.resolve(*sym) {
            names.insert(s);
        }
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
                hir.idents[td.name].text.as_str().hash(&mut hasher);
                for g in &td.generics {
                    hir.idents[*g].text.as_str().hash(&mut hasher);
                }
                0u8.hash(&mut hasher);
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    hir.idents[attr.name].text.as_str().hash(&mut hasher);
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
                    hir.idents[fnd.name].text.as_str().hash(&mut hasher);
                    for g in &fnd.generics {
                        hir.idents[*g].text.as_str().hash(&mut hasher);
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
                hir.idents[ed.name].text.as_str().hash(&mut hasher);
                for f in &ed.fields {
                    hir.idents[hir.enum_fields[*f].name]
                        .text
                        .as_str()
                        .hash(&mut hasher);
                }
                0u8.hash(&mut hasher);
            }
            Decl::Fn(fnd) => {
                3u8.hash(&mut hasher);
                hir.idents[fnd.name].text.as_str().hash(&mut hasher);
                for g in &fnd.generics {
                    hir.idents[*g].text.as_str().hash(&mut hasher);
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
                hir.idents[vd.name].text.as_str().hash(&mut hasher);
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
    hir.idents[r.name].text.as_str().hash(hasher);
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
    arena_mut: &mut greycat_analyzer_types::TypeArena,
    index: &mut ProjectIndex,
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
        let entry = lower_module_signatures(arena_mut, index, uri, hir, sig_hash, name_set_hash);
        apply_module_contributions(index, &entry);
        cache.insert((*uri).clone(), entry);
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
    arena_mut: &mut greycat_analyzer_types::TypeArena,
    index: &mut ProjectIndex,
    uri: &Uri,
    hir: &Hir,
    sig_hash: u64,
    name_set_hash: u64,
) -> ModuleSigCache {
    use greycat_analyzer_types::{GenericOwner, Symbol};

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
                let type_name_text = hir.idents[td.name].text.as_str();
                let type_sym = index.symbols.intern(type_name_text);
                let owner = GenericOwner::Type(type_name_text.into());
                let mut generics_in_scope: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                for g in &td.generics {
                    let g_sym = index.symbols.intern(hir.idents[*g].text.as_str());
                    generics_in_scope.insert(g_sym, owner.clone());
                }
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    let attr_sym = index.symbols.intern(hir.idents[attr.name].text.as_str());
                    let Some(tr) = attr.ty else {
                        continue;
                    };
                    let ty =
                        lower_type_ref_project(hir, tr, arena_mut, &*index, &generics_in_scope);
                    entry.attrs.push((type_sym, attr_sym, ty));
                }
                for method_id in &td.methods {
                    let Decl::Fn(fnd) = &hir.decls[*method_id] else {
                        continue;
                    };
                    let method_text = hir.idents[fnd.name].text.as_str();
                    let method_sym = index.symbols.intern(method_text);
                    let Some(ret) = fnd.return_type else {
                        continue;
                    };
                    // P19.8: push the method's generics onto the
                    // type-level scope, lower, then pop. Avoids
                    // cloning `generics_in_scope` (a HashMap with
                    // GenericOwner-owned Strings) per method —
                    // overrides of the outer scope are saved and
                    // restored.
                    let method_owner = GenericOwner::Function(method_text.into());
                    let mut saved: Vec<(Symbol, Option<GenericOwner>)> =
                        Vec::with_capacity(fnd.generics.len());
                    for g in &fnd.generics {
                        let g_sym = index.symbols.intern(hir.idents[*g].text.as_str());
                        let prev = generics_in_scope.insert(g_sym, method_owner.clone());
                        saved.push((g_sym, prev));
                    }
                    let ty =
                        lower_type_ref_project(hir, ret, arena_mut, &*index, &generics_in_scope);
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
                    entry.methods.push((type_sym, method_sym, ty));
                }
            }
            Decl::Enum(ed) => {
                let name_text = hir.idents[ed.name].text.as_str();
                let name_sym = index.symbols.intern(name_text);
                let variants: Vec<SmolStr> = ed
                    .fields
                    .iter()
                    .map(|f| hir.idents[hir.enum_fields[*f].name].text.as_str().into())
                    .collect();
                let enum_id = arena_mut.alloc(greycat_analyzer_types::Type {
                    kind: greycat_analyzer_types::TypeKind::Enum {
                        name: name_text.into(),
                        variants,
                    },
                    nullable: false,
                });
                entry.enums.push((name_sym, enum_id));
            }
            Decl::Fn(fnd) => {
                let fn_text = hir.idents[fnd.name].text.as_str();
                let fn_sym = index.symbols.intern(fn_text);
                let Some(ret) = fnd.return_type else {
                    continue;
                };
                let owner = GenericOwner::Function(fn_text.into());
                let mut generics_in_scope: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                let mut generics: Vec<Symbol> = Vec::with_capacity(fnd.generics.len());
                for g in &fnd.generics {
                    let g_sym = index.symbols.intern(hir.idents[*g].text.as_str());
                    generics_in_scope.insert(g_sym, owner.clone());
                    generics.push(g_sym);
                }
                let ret_ty =
                    lower_type_ref_project(hir, ret, arena_mut, &*index, &generics_in_scope);
                // **P19.15** — also pre-lower parameter types so the
                // analyzer's generic-call inference can run on
                // cross-module callees (`abs`, `min`, `max`, …).
                let mut params: Vec<greycat_analyzer_types::TypeId> =
                    Vec::with_capacity(fnd.params.len());
                for p_id in &fnd.params {
                    let p = &hir.fn_params[*p_id];
                    let pt = if let Some(tr) = p.ty {
                        lower_type_ref_project(hir, tr, arena_mut, &*index, &generics_in_scope)
                    } else {
                        arena_mut.any()
                    };
                    params.push(pt);
                }
                entry.fns.push((
                    fn_sym,
                    FnSignature {
                        home_uri: uri.clone(),
                        return_ty: ret_ty,
                        generics,
                        params,
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
                let var_text = hir.idents[vd.name].text.as_str();
                let var_sym = index.symbols.intern(var_text);
                let Some(tr) = vd.ty else {
                    continue;
                };
                // Vars never declare generics, so no scope needed.
                let empty: FxHashMap<Symbol, GenericOwner> = FxHashMap::default();
                let var_ty = lower_type_ref_project(hir, tr, arena_mut, &*index, &empty);
                entry.vars.push((var_sym, var_ty));
            }
            _ => {}
        }
    }
    entry
}

// P19.14
/// Index-aware extension of [`greycat_analyzer_types::is_assignable_to`]
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
fn is_assignable_to_with_index(
    index: &ProjectIndex,
    arena: &greycat_analyzer_types::TypeArena,
    from: greycat_analyzer_types::TypeId,
    to: greycat_analyzer_types::TypeId,
) -> bool {
    use greycat_analyzer_types::TypeKind;
    if greycat_analyzer_types::is_assignable_to(arena, from, to) {
        return true;
    }
    let a = arena.get(from);
    let b = arena.get(to);
    if a.nullable && !b.nullable {
        return false;
    }
    match (&a.kind, &b.kind) {
        (TypeKind::Named { name: na }, TypeKind::Named { name: nb }) => index.is_subtype_of(na, nb),
        (TypeKind::Generic { name: na, args: aa }, TypeKind::Generic { name: nb, args: ab })
            if na == "node" && nb == "node" && aa.len() == 1 && ab.len() == 1 =>
        {
            // node<Sub> -> node<Super> when Sub extends Super (or
            // identical). Recurse so a chain like
            // node<DeepSub> -> node<MidSub> -> node<Super> still
            // works in one hop.
            is_assignable_to_with_index(index, arena, aa[0], ab[0])
        }
        _ => false,
    }
}

// P19.6
/// Apply a cached / freshly-built module contribution to
/// the project index. Mirrors the apply-loop the original
/// `lower_signatures_into` ran at end-of-pass: `or_insert` semantics
/// preserve the "first decl wins" collision rule that the rest of
/// the pipeline assumes.
fn apply_module_contributions(index: &mut ProjectIndex, c: &ModuleSigCache) {
    for (type_sym, attr_sym, ty) in &c.attrs {
        if let Some(tm) = index.type_members.get_mut(type_sym) {
            tm.attr_types.insert(*attr_sym, *ty);
        }
    }
    for (type_sym, method_sym, ty) in &c.methods {
        if let Some(tm) = index.type_members.get_mut(type_sym) {
            tm.method_returns.insert(*method_sym, *ty);
        }
    }
    for (fn_sym, sig) in &c.fns {
        index
            .fn_signatures
            .entry(*fn_sym)
            .or_insert_with(|| sig.clone());
    }
    for (sym, ty) in &c.enums {
        index.enum_types.entry(*sym).or_insert(*ty);
    }
    for (sym, ty) in &c.vars {
        index.var_types.entry(*sym).or_insert(*ty);
    }
}

impl ProjectAnalysis {
    /// **Stages S2-S11 + S12 (per-module slice).** Currently delegates
    /// to `analyze_with_index_into`, which combines name declaration,
    /// structure declaration, signature lowering, and body walking
    /// inside `Cx::visit_decl`. Subsequent extraction passes will split
    /// out S2-S6, S7-S11, and S12 — at which point this stage shrinks
    /// to a thin "wire it all together" call.
    fn stage_per_module_analysis(&mut self, hirs: Vec<(Uri, Hir, String, Duration, Directives)>) {
        let bypass = self.bypass_suppressions;
        let index = &self.index;

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

        let pass_a_run = |(uri, hir, lib, lower_took, mut directives): (
            Uri,
            Hir,
            String,
            Duration,
            Directives,
        )|
         -> PassAOut {
            let t0 = Instant::now();
            let resolutions = resolve_with_index(&hir, index);
            let resolve_took = t0.elapsed();
            let t2 = Instant::now();
            // Seed `lints` with the directive parser's own diagnostics
            // (`unknown-suppression-rule`, `empty-suppression`, …) so
            // they ride alongside regular lints into LSP / CLI surfaces.
            let mut lints = std::mem::take(&mut directives.diagnostics);
            lints.extend(run_lints_with_directives(
                &hir,
                &resolutions,
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
            let analysis = analyze_with_index_into(
                &p.hir,
                &p.resolutions,
                &self.index,
                &self.well_known,
                &self.decl_registry,
                &p.uri,
                &mut self.arena,
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

    /// Walk every module's `Expr::Call` and emit a diagnostic for
    /// Walks every module's `Expr::Call`, resolves the callee to its
    /// declared `FnDecl` (in-module via `Resolutions::uses` + `member_uses`,
    /// cross-module via `foreign_member_uses` + `QualifiedStatic`),
    /// and emits a `value of type X is not assignable to parameter Y`
    /// diagnostic for each mismatched arg.
    ///
    /// Runs after pass 3.5 so the arg-side `expr_types` reflect any
    /// cross-module return-type inferences (otherwise outer calls
    /// whose args are inner static-expr calls would all surface
    /// "value of type `any`" false positives).
    /// Walk every module's `Expr::Call` and emit a diagnostic for
    /// each arg whose settled type isn't assignable to the
    /// corresponding declared param. Folded into the unified
    /// validation phase so all type-relation diagnostics share one
    /// producer.
    // P19
    /// Split-borrow variant: takes `&modules`, `&index`, and a
    /// mutable borrow on the shared arena. The validation loop holds
    /// `&mut self.arena` during iteration over `&self.modules`, so the
    /// `&self`-borrowing version can no longer be invoked directly
    /// from the same scope.
    #[allow(clippy::mutable_key_type)]
    fn collect_call_arg_diags_split(
        modules: &FxHashMap<Uri, ModuleAnalysis>,
        index: &ProjectIndex,
        cur_uri: &Uri,
        arena: &mut greycat_analyzer_types::TypeArena,
    ) -> Vec<crate::analyzer::SemanticDiagnostic> {
        use crate::analyzer::{DiagCategory, SemanticDiagnostic, Severity};
        use greycat_analyzer_hir::types::Expr;

        let cur_module = match modules.get(cur_uri) {
            Some(m) => m,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for (_call_id, call_expr) in cur_module.hir.exprs.iter() {
            let Expr::Call(call) = call_expr else {
                continue;
            };
            let Some((foreign_uri_opt, fn_decl_id)) =
                resolve_call_target(modules, index, cur_module, call.callee)
            else {
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
                let fn_name = fn_module.hir.idents[fnd.name].text.clone();
                let callee_end = cur_module.hir.exprs[call.callee].byte_range().end;
                let plural = if expected == 1 { "" } else { "s" };
                out.push(SemanticDiagnostic {
                    severity: Severity::Error,
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
                let declared_raw = lower_type_ref_project(
                    &fn_module.hir,
                    declared_ref,
                    arena,
                    index,
                    &method_generics_in_scope,
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
                if !is_assignable_to_with_index(index, arena, arg_ty, declared_ty) {
                    let p_name = fn_module.hir.idents[p.name].text.clone();
                    let arg_display = greycat_analyzer_types::display(arena, arg_ty);
                    let declared_display = greycat_analyzer_types::display(arena, declared_ty);
                    let r = cur_module.hir.exprs[call.args[i]].byte_range();
                    out.push(SemanticDiagnostic {
                        severity: Severity::Error,
                        message: format!(
                            "value of type `{}` is not assignable to parameter `{}: {}`",
                            arg_display, p_name, declared_display
                        ),
                        byte_range: r,
                        category: DiagCategory::TypeRelation,
                    });
                }
            }
        }
        out
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
        let doc_data: FxHashMap<Uri, (String, greycat_analyzer_syntax::tree_sitter::Tree)> = self
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
            run_typed_lints_for_module(uri, module, arena, index, decl_registry, bypass, &doc_data);
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
        for (cur_uri, cur_module) in &self.modules {
            if !in_scope(cur_uri) {
                continue;
            }
            let mut diags: Vec<SemanticDiagnostic> = Vec::new();
            validate_module_type_relations(cur_module, index, arena_mut, &mut diags);
            // Call-arg validation needs cross-module access (foreign
            // fn signatures), so it lives on `&self` rather than the
            // free walker. Note: we hold `arena_mut` here, so call into
            // a helper that accepts `&self.modules` + `&self.index` +
            // `arena` instead of borrowing `&self`.
            diags.extend(Self::collect_call_arg_diags_split(
                &self.modules,
                index,
                cur_uri,
                arena_mut,
            ));
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
                        && target_module.hir.idents[name_idx].text == *needle
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
        let changed_hir = manager.get(uri).map(|cell| {
            let doc = cell.borrow();
            let start = Instant::now();
            // P35.1 — module name from URI for the well-known recogniser.
            let module_name =
                crate::stdlib::module_name_from_uri(uri).unwrap_or_else(|| "module".to_string());
            let hir = lower_module(&doc.text, module_name, &doc.lib, doc.root_node());
            lower_took = start.elapsed();
            changed_lib = Some(doc.lib.clone());
            changed_directives = Some(crate::directives::parse_directives(
                &doc.text,
                doc.root_node(),
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
        let mut new_index = ProjectIndex::with_symbols(preserved_symbols);
        if let Some(hir) = &changed_hir {
            new_index.ingest(uri, hir);
        }
        for (other_uri, ma) in &self.modules {
            if other_uri == uri {
                continue;
            }
            new_index.ingest(other_uri, &ma.hir);
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
            let module_name = crate::stdlib::module_name_from_uri(other_uri)
                .unwrap_or_else(|| "module".to_string());
            let hir = lower_module(&doc.text, module_name, &doc.lib, doc.root_node());
            new_index.ingest(other_uri, &hir);
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
                &pairs,
                &mut self.sig_cache,
            );
        }

        let mut timings = ModuleTimings {
            lower: lower_took,
            ..ModuleTimings::default()
        };
        let t0 = Instant::now();
        let resolutions = resolve_with_index(&hir, &self.index);
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
        lints.extend(run_lints_with_directives(
            &hir,
            &resolutions,
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
        // P22-P23 — passes 3.4 / 3.45 / 3.5 / 3.52 are gone; cross-
        // module typing happens inline in the analyzer's body walker.
        // Only the typed-lint pass and type-relation validation remain
        // — both run on the changed URI only here for incremental cost.
        let mut touched: FxHashSet<&str> = FxHashSet::default();
        touched.insert(uri.as_str());
        self.run_typed_lints(manager, Some(&touched));
        self.validate_type_relations(Some(&touched));
        self.compute_qualified_refs(manager);
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
fn qualified_chain(
    node: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    text: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    collect_chain(node, text, &mut out);
    out
}

fn collect_chain(
    node: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    text: &str,
    out: &mut Vec<String>,
) {
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
/// Lambda callees and unresolved member accesses return `None`.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_call_target(
    modules: &FxHashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    callee: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
) -> Option<(Option<Uri>, Idx<Decl>)> {
    use crate::analyzer::MemberDef;
    use crate::resolver::Definition;
    use greycat_analyzer_hir::types::Expr;

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
    property: Idx<greycat_analyzer_hir::types::Ident>,
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
    chain: &[Idx<greycat_analyzer_hir::types::Ident>],
) -> Option<(Uri, Idx<Decl>, QualifiedTarget)> {
    use crate::analyzer::MemberDef;
    if chain.len() != 3 {
        return None;
    }
    let module_name = cur.hir.idents[chain[0]].text.as_str();
    let type_name = cur.hir.idents[chain[1]].text.as_str();
    let member_name = cur.hir.idents[chain[2]].text.as_str();
    let module_uri = index.module_uri(module_name)?.clone();
    let foreign = modules.get(&module_uri)?;
    let foreign_root = foreign.hir.module.as_ref()?;
    // Look for the named decl — could be a `type` or `enum`.
    let mut found: Option<Idx<Decl>> = None;
    for decl_id in &foreign_root.decls {
        let name_text = match &foreign.hir.decls[*decl_id] {
            Decl::Type(td) => &foreign.hir.idents[td.name].text,
            Decl::Enum(ed) => &foreign.hir.idents[ed.name].text,
            _ => continue,
        };
        if name_text == type_name {
            found = Some(*decl_id);
            break;
        }
    }
    let type_decl_id = found?;
    match &foreign.hir.decls[type_decl_id] {
        Decl::Enum(ed) => {
            for f in &ed.fields {
                if foreign.hir.idents[foreign.hir.enum_fields[*f].name].text == member_name {
                    return Some((module_uri, type_decl_id, QualifiedTarget::EnumVariant));
                }
            }
            None
        }
        Decl::Type(td) => {
            for attr_id in &td.attrs {
                if foreign.hir.idents[foreign.hir.type_attrs[*attr_id].name].text == member_name {
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
                if foreign.hir.idents[m.name].text == member_name {
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
#[allow(clippy::mutable_key_type)]
fn run_typed_lints_for_module(
    uri: &Uri,
    module: &mut ModuleAnalysis,
    arena: &greycat_analyzer_types::TypeArena,
    index: &ProjectIndex,
    decl_registry: &crate::well_known::DeclRegistry,
    bypass: bool,
    doc_data: &FxHashMap<Uri, (String, greycat_analyzer_syntax::tree_sitter::Tree)>,
) {
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
        )
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
    if !bypass {
        lint_unused_suppressions(&mut module.directives, &mut module.lints);
    }
}

/// the analyzer's per-module pass deferred. Reads only — never
/// mutates `module`. The shared project arena is passed in;
/// any newly-needed declared-side TypeIds are minted into it
/// alongside everything else, which is fine because the arena is
/// append-only and intern-collapsed.
fn validate_module_type_relations(
    module: &ModuleAnalysis,
    index: &ProjectIndex,
    arena: &mut greycat_analyzer_types::TypeArena,
    diags: &mut Vec<crate::analyzer::SemanticDiagnostic>,
) {
    use crate::analyzer::{SemanticDiagnostic, Severity};
    use greycat_analyzer_hir::types::Decl;
    use greycat_analyzer_types::Primitive;

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
            index,
            arena,
            bool_t,
            &hir.decls[*d_id],
            diags,
        );
    }

    fn validate_decl(
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
        index: &ProjectIndex,
        arena: &mut greycat_analyzer_types::TypeArena,
        bool_t: greycat_analyzer_types::TypeId,
        decl: &Decl,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        match decl {
            Decl::Fn(fnd) => {
                let return_ty = fnd
                    .return_type
                    .and_then(|t| lower_type_ref_id(hir, t, &analysis.registry, arena));
                if let Some(body) = fnd.body {
                    validate_stmt(hir, analysis, index, arena, bool_t, body, return_ty, diags);
                }
            }
            Decl::Type(td) => {
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    if let (Some(decl_ref), Some(init)) = (attr.ty, attr.init)
                        && let Some(declared_ty) =
                            lower_type_ref_id(hir, decl_ref, &analysis.registry, arena)
                    {
                        check_assign(
                            analysis,
                            index,
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
                    validate_decl(hir, analysis, index, arena, bool_t, &hir.decls[*m], diags);
                }
            }
            Decl::Var(vd) => {
                if let (Some(decl_ref), Some(init)) = (vd.ty, vd.init)
                    && let Some(declared_ty) =
                        lower_type_ref_id(hir, decl_ref, &analysis.registry, arena)
                {
                    check_assign(
                        analysis,
                        index,
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
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
        index: &ProjectIndex,
        arena: &mut greycat_analyzer_types::TypeArena,
        bool_t: greycat_analyzer_types::TypeId,
        block: &greycat_analyzer_hir::types::BlockStmt,
        return_ty: Option<greycat_analyzer_types::TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        for s in &block.stmts {
            validate_stmt(hir, analysis, index, arena, bool_t, *s, return_ty, diags);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_stmt(
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
        index: &ProjectIndex,
        arena: &mut greycat_analyzer_types::TypeArena,
        bool_t: greycat_analyzer_types::TypeId,
        stmt_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Stmt>,
        return_ty: Option<greycat_analyzer_types::TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        use greycat_analyzer_hir::types::{
            AssignStmt, AtStmt, DoWhileStmt, ForInStmt, ForStmt, IfStmt, LocalVar, Stmt, TryStmt,
            WhileStmt,
        };
        match &hir.stmts[stmt_id] {
            Stmt::Block(b) => {
                validate_block(hir, analysis, index, arena, bool_t, b, return_ty, diags)
            }
            Stmt::Var(LocalVar { ty, init, .. }) => {
                if let (Some(decl_ref), Some(init_id)) = (ty, init)
                    && let Some(declared_ty) =
                        lower_type_ref_id(hir, *decl_ref, &analysis.registry, arena)
                {
                    let r = expr_byte_range(hir, *init_id);
                    check_assign(
                        analysis,
                        index,
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
                    *condition,
                    bool_t,
                    "if condition",
                    hir,
                    diags,
                );
                validate_block(
                    hir,
                    analysis,
                    index,
                    arena,
                    bool_t,
                    then_branch,
                    return_ty,
                    diags,
                );
                if let Some(eb) = else_branch {
                    validate_stmt(hir, analysis, index, arena, bool_t, *eb, return_ty, diags);
                }
            }
            Stmt::While(WhileStmt {
                condition, body, ..
            }) => {
                check_bool(
                    analysis,
                    arena,
                    *condition,
                    bool_t,
                    "while condition",
                    hir,
                    diags,
                );
                validate_block(hir, analysis, index, arena, bool_t, body, return_ty, diags);
            }
            Stmt::DoWhile(DoWhileStmt {
                condition, body, ..
            }) => {
                check_bool(
                    analysis,
                    arena,
                    *condition,
                    bool_t,
                    "do-while condition",
                    hir,
                    diags,
                );
                validate_block(hir, analysis, index, arena, bool_t, body, return_ty, diags);
            }
            Stmt::For(ForStmt {
                condition, body, ..
            }) => {
                if let Some(c) = condition {
                    check_bool(analysis, arena, *c, bool_t, "for condition", hir, diags);
                }
                validate_block(hir, analysis, index, arena, bool_t, body, return_ty, diags);
            }
            Stmt::ForIn(ForInStmt { body, .. }) => {
                validate_block(hir, analysis, index, arena, bool_t, body, return_ty, diags);
            }
            Stmt::Try(TryStmt {
                try_block,
                catch_block,
                ..
            }) => {
                validate_block(
                    hir, analysis, index, arena, bool_t, try_block, return_ty, diags,
                );
                validate_block(
                    hir,
                    analysis,
                    index,
                    arena,
                    bool_t,
                    catch_block,
                    return_ty,
                    diags,
                );
            }
            Stmt::At(AtStmt { block, .. }) => {
                validate_block(hir, analysis, index, arena, bool_t, block, return_ty, diags);
            }
            Stmt::Return(Some(v)) => {
                if let Some(rt) = return_ty {
                    let r = expr_byte_range(hir, *v);
                    check_assign(
                        analysis,
                        index,
                        arena,
                        *v,
                        rt,
                        "return value",
                        "declared return type",
                        r,
                        diags,
                    );
                }
            }
            Stmt::Return(None) | Stmt::Expr(_) | Stmt::Break | Stmt::Continue | Stmt::Throw(_) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_assign(
        analysis: &crate::analyzer::AnalysisResult,
        index: &ProjectIndex,
        arena: &greycat_analyzer_types::TypeArena,
        value_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
        declared_ty: greycat_analyzer_types::TypeId,
        value_label: &str,
        target_label: &str,
        range: std::ops::Range<usize>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        let Some(value_ty) = analysis.expr_types.get(&value_id).copied() else {
            return;
        };
        if is_assignable_to_with_index(index, arena, value_ty, declared_ty) {
            return;
        }
        let got = greycat_analyzer_types::display(arena, value_ty);
        let want = greycat_analyzer_types::display(arena, declared_ty);
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            message: format!(
                "{value_label} of type `{got}` is not assignable to {target_label} `{want}`"
            ),
            byte_range: range,
            category: crate::analyzer::DiagCategory::TypeRelation,
        });
    }

    fn check_bool(
        analysis: &crate::analyzer::AnalysisResult,
        arena: &greycat_analyzer_types::TypeArena,
        expr_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
        bool_t: greycat_analyzer_types::TypeId,
        label: &'static str,
        hir: &greycat_analyzer_hir::Hir,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        let Some(ty) = analysis.expr_types.get(&expr_id).copied() else {
            return;
        };
        if greycat_analyzer_types::is_assignable_to(arena, ty, bool_t) {
            return;
        }
        let got = greycat_analyzer_types::display(arena, ty);
        diags.push(SemanticDiagnostic {
            severity: Severity::Error,
            message: format!("{label} must be `bool`, got `{got}`"),
            byte_range: expr_byte_range(hir, expr_id),
            category: crate::analyzer::DiagCategory::TypeRelation,
        });
    }

    fn expr_byte_range(
        hir: &greycat_analyzer_hir::Hir,
        expr_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
    ) -> std::ops::Range<usize> {
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
    type_ref: Idx<greycat_analyzer_hir::types::TypeRef>,
    arena: &mut greycat_analyzer_types::TypeArena,
    index: &ProjectIndex,
    generics_in_scope: &FxHashMap<
        greycat_analyzer_types::Symbol,
        greycat_analyzer_types::GenericOwner,
    >,
) -> greycat_analyzer_types::TypeId {
    use greycat_analyzer_types::Primitive;
    let tr = hir.type_refs[type_ref].clone();
    let name = hir.idents[tr.name].text.as_str();
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
                let args: Vec<greycat_analyzer_types::TypeId> = tr
                    .params
                    .iter()
                    .map(|p| lower_type_ref_project(hir, *p, arena, index, generics_in_scope))
                    .collect();
                arena.generic(name.to_string(), args)
            } else if let Some(sym) = index.symbol(name)
                && let Some(owner) = generics_in_scope.get(&sym)
            {
                arena.generic_param(name.to_string(), owner.clone())
            } else if let Some(enum_id) = index.enum_type_for(name) {
                // P19.10 — canonical enum TypeId from S7-S11.
                // Without this, a cross-module enum reference would
                // mint `Named(name)` (kind != Enum), which breaks
                // the analyzer's `Static` enum-variant arm
                // (`if let TypeKind::Enum { variants, .. } = ...`).
                enum_id
            } else if index.has_name(name) {
                arena.named(name.to_string())
            } else {
                // P35.3 — unknown type. Was `any()`; now `Unresolved`
                // so hover / display surface the typo'd name.
                arena.unresolved(name.to_string(), (tr.byte_range.start, tr.byte_range.end))
            }
        }
    };
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}

/// Look up a syntactic `TypeRef` and mint a corresponding `TypeId`
/// into `arena`. `arena` is the validation-pass's working clone of
/// `analysis.types`, so any new mints land where `is_assignable_to`
/// can see them.
fn lower_type_ref_id(
    hir: &greycat_analyzer_hir::Hir,
    type_ref: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeRef>,
    registry: &greycat_analyzer_types::TypeRegistry,
    arena: &mut greycat_analyzer_types::TypeArena,
) -> Option<greycat_analyzer_types::TypeId> {
    use greycat_analyzer_types::Primitive;
    let tr = &hir.type_refs[type_ref];
    let name = hir.idents[tr.name].text.as_str();
    let base = match name {
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
                let mut args = Vec::with_capacity(tr.params.len());
                for p in &tr.params {
                    args.push(lower_type_ref_id(hir, *p, registry, arena)?);
                }
                arena.generic(name.to_string(), args)
            } else if let Some(id) = registry.lookup(name) {
                id
            } else {
                arena.named(name.to_string())
            }
        }
    };
    Some(if tr.optional {
        arena.nullable(base)
    } else {
        base
    })
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
type GenericsInScope =
    FxHashMap<greycat_analyzer_types::Symbol, greycat_analyzer_types::GenericOwner>;
type MethodSubst = FxHashMap<String, greycat_analyzer_types::TypeId>;

fn method_subst_from_receiver(
    arena: &greycat_analyzer_types::TypeArena,
    cur_module: &ModuleAnalysis,
    fn_module: &ModuleAnalysis,
    index: &ProjectIndex,
    callee_expr: &greycat_analyzer_hir::types::Expr,
) -> (GenericsInScope, MethodSubst) {
    use greycat_analyzer_hir::types::Expr;
    use greycat_analyzer_types::{GenericOwner, TypeKind};
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
    let (recv_name, recv_args): (&str, &[greycat_analyzer_types::TypeId]) = match &recv.kind {
        TypeKind::Generic { name, args } => (name.as_str(), args.as_slice()),
        _ => return empty(),
    };
    let Some(module) = fn_module.hir.module.as_ref() else {
        return empty();
    };
    let mut owner_td: Option<&greycat_analyzer_hir::types::TypeDecl> = None;
    for d_id in &module.decls {
        if let Decl::Type(td) = &fn_module.hir.decls[*d_id]
            && fn_module.hir.idents[td.name].text.as_str() == recv_name
        {
            owner_td = Some(td);
            break;
        }
    }
    let Some(td) = owner_td else {
        return empty();
    };
    let owner = GenericOwner::Type(recv_name.into());
    let mut generics_in_scope: GenericsInScope = FxHashMap::default();
    let mut subst: MethodSubst = FxHashMap::default();
    for (i, gen_idx) in td.generics.iter().enumerate() {
        let gname = fn_module.hir.idents[*gen_idx].text.as_str();
        // `lower_type_ref_project` reads `generics_in_scope` keyed by
        // Symbol; only names *already* in the project's symbol table
        // get the GenericParam treatment. Use `lookup` (read-only) —
        // mutating the table here would cross the `&ProjectIndex`
        // borrow.
        if let Some(sym) = index.symbol(gname) {
            generics_in_scope.insert(sym, owner.clone());
        }
        if let Some(arg_id) = recv_args.get(i).copied() {
            subst.insert(gname.to_string(), arg_id);
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
        assert!(
            pa.index.registry.lookup("Point").is_some(),
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
        let cached_attrs_before = pa
            .index
            .type_members_for("Pair")
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
            .type_members_for("Pair")
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
        assert!(pa.index.contains_fn_signature("a"));

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
            .fn_signature_for("a")
            .expect("a sig present after invalidate");
        let display = greycat_analyzer_types::display(&pa.arena, sig.return_ty);
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
}
