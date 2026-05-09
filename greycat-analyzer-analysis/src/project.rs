//! Project-level analyzer driver (P6.1).
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
//! consult it for cross-module name lookup is **P6.2** territory. P6.1
//! gives that work the cache-shaped seam to plug into.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Decl;
use greycat_analyzer_hir::{Hir, lower_module};

use crate::analyzer::{AnalysisResult, analyze_with_index_into, seed_builtins};
use crate::lint::{LintDiagnostic, lint_arrow_on_non_deref, run_lints};
use crate::resolver::{Resolutions, resolve_with_index};
use crate::stdlib::ProjectIndex;

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
    /// P14.5 — per-phase wall-clock timings captured during the
    /// last `rebuild` / `invalidate`. Useful for surfacing where the
    /// pipeline spends its time (`cli lint --csv`).
    pub timings: ModuleTimings,
}

/// P14.5 — per-module pipeline timings.
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
/// **P19:** the [`greycat_analyzer_types::TypeArena`] now lives on the
/// project (not per [`AnalysisResult`]). Every module's analyzer mints
/// into the same arena so cross-module `TypeId`s are directly
/// comparable — no `mint_type_shape`/`read_type_shape` translation
/// needed. Callers that previously wrote `module.analysis.types`
/// should call [`Self::arena`] / [`Self::arena_mut`] instead.
#[derive(Debug, Default)]
pub struct ProjectAnalysis {
    pub index: ProjectIndex,
    /// P19 — project-wide type arena. Populated alongside every
    /// module's analyzer pass. Append-only and interned, so duplicate
    /// `seed_builtins` calls per `analyze_with_index_into` are a
    /// no-op.
    pub arena: greycat_analyzer_types::TypeArena,
    modules: HashMap<Uri, ModuleAnalysis>,
}

impl ProjectAnalysis {
    pub fn new() -> Self {
        let mut arena = greycat_analyzer_types::TypeArena::new();
        seed_builtins(&mut arena);
        Self {
            index: ProjectIndex::new(),
            arena,
            modules: HashMap::new(),
        }
    }

    /// Borrow the project-wide type arena — required for any
    /// `TypeId` lookup (`arena.get(id)`, `display(arena, id)`, …).
    pub fn arena(&self) -> &greycat_analyzer_types::TypeArena {
        &self.arena
    }

    /// Mutable borrow of the project-wide type arena. Capability
    /// handlers should not mint new types; this is reserved for the
    /// orchestrator and the staged-pipeline body walker (P22+).
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
    /// **P20:** routes through [`Self::analyze_staged`], the staged-
    /// pipeline orchestrator that future phases (P21-P23) will fill in
    /// stage-by-stage. For now, every stage delegates to the existing
    /// per-module + post-pass machinery — same shape, named seam.
    pub fn rebuild(&mut self, manager: &SourceManager) {
        self.analyze_staged(manager);
    }

    /// **P20** — staged-pipeline entry point. The 12-stage design from
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
    /// **P20 status:** the staged interface exists but each stage
    /// delegates to the existing per-module + post-pass machinery.
    /// Stages get filled in incrementally:
    /// - **P21** extracts S1-S6 (deletes pass 3 `resolve_cross_module_members`)
    /// - **P22** extracts S7-S11 (deletes passes 3.4 / 3.45 / 3.52 +
    ///   the receiver-driven substitution shim)
    /// - **P23** rewrites S12 (deletes passes 3.5 / 3.55 / 3.6 +
    ///   the propagate / fixed-point cascade)
    /// - **P24** wires the Q1-Q5 query DAG so `invalidate(uri)` only
    ///   replays Q5(uri) (and Q4 / Q5(others) when signatures change)
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
        self.stage_cross_module_post_passes();

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
    }

    /// **Stage S1** — parse + lower every module to HIR, ingest into
    /// the project index. Returns the lowered modules in document-order
    /// so [`Self::stage_per_module_analysis`] can move them into the
    /// per-module cache without re-lowering.
    fn stage_lower(&mut self, manager: &SourceManager) -> Vec<(Uri, Hir, String, Duration)> {
        let mut hirs: Vec<(Uri, Hir, String, Duration)> = Vec::with_capacity(manager.len());
        for (uri, cell) in manager.iter() {
            let doc = cell.borrow();
            let lower_start = Instant::now();
            let hir = lower_module(&doc.text, "module", &doc.lib, doc.root_node());
            let lower_took = lower_start.elapsed();
            self.index.ingest(uri, &hir);
            hirs.push((uri.clone(), hir, doc.lib.clone(), lower_took));
        }
        hirs
    }

    /// **Stages S7-S11** (P22) — lower every type's attr `TypeRef`s
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
    fn stage_lower_signatures(&mut self, lowered: &[(Uri, Hir, String, Duration)]) {
        use greycat_analyzer_hir::types::Decl;
        use greycat_analyzer_types::GenericOwner;

        // Split borrow: arena + index.type_members are both fields of
        // `self`, but distinct, so direct field access lets us hold
        // both mutably while reading `index` immutably elsewhere.
        let arena_mut = &mut self.arena;
        let index_ref = &self.index;
        // Buffer the writes; we need `&self.index` for cross-module
        // name lookup *during* lowering, then apply the writes after.
        type AttrUpdate = (String, String, greycat_analyzer_types::TypeId);
        type MethodUpdate = (String, String, greycat_analyzer_types::TypeId);
        let mut attr_updates: Vec<AttrUpdate> = Vec::new();
        let mut method_updates: Vec<MethodUpdate> = Vec::new();

        for (_uri, hir, _lib, _) in lowered {
            let Some(module) = hir.module.as_ref() else {
                continue;
            };
            for d_id in &module.decls {
                let Decl::Type(td) = &hir.decls[*d_id] else {
                    continue;
                };
                let type_name = hir.idents[td.name].text.clone();
                // Build the generics scope for this type.
                let owner = GenericOwner::Type(type_name.clone());
                let mut generics_in_scope: HashMap<String, GenericOwner> = HashMap::new();
                for g in &td.generics {
                    generics_in_scope.insert(hir.idents[*g].text.clone(), owner.clone());
                }
                // Lower attrs.
                for attr_id in &td.attrs {
                    let attr = &hir.type_attrs[*attr_id];
                    let attr_name = hir.idents[attr.name].text.clone();
                    let Some(tr) = attr.ty else {
                        continue;
                    };
                    let ty =
                        lower_type_ref_project(hir, tr, arena_mut, index_ref, &generics_in_scope);
                    attr_updates.push((type_name.clone(), attr_name, ty));
                }
                // Lower method returns.
                for method_id in &td.methods {
                    let Decl::Fn(fnd) = &hir.decls[*method_id] else {
                        continue;
                    };
                    let method_name = hir.idents[fnd.name].text.clone();
                    let Some(ret) = fnd.return_type else {
                        continue;
                    };
                    // Method bodies see *both* the type's generics and
                    // any of the method's own (`fn<U>(...)`).
                    let mut method_generics = generics_in_scope.clone();
                    let method_owner = GenericOwner::Function(method_name.clone());
                    for g in &fnd.generics {
                        method_generics.insert(hir.idents[*g].text.clone(), method_owner.clone());
                    }
                    let ty =
                        lower_type_ref_project(hir, ret, arena_mut, index_ref, &method_generics);
                    method_updates.push((type_name.clone(), method_name, ty));
                }
            }
        }
        // Apply writes — `index_ref` and `arena_mut` borrows end here.
        for (type_name, attr_name, ty) in attr_updates {
            if let Some(tm) = self.index.type_members.get_mut(&type_name) {
                tm.attr_types.insert(attr_name, ty);
            }
        }
        for (type_name, method_name, ty) in method_updates {
            if let Some(tm) = self.index.type_members.get_mut(&type_name) {
                tm.method_returns.insert(method_name, ty);
            }
        }
    }

    /// **Stages S2-S11 + S12 (per-module slice).** Currently delegates
    /// to `analyze_with_index_into`, which combines name declaration,
    /// structure declaration, signature lowering, and body walking
    /// inside `Cx::visit_decl`. P21 extracts S2-S6, P22 extracts
    /// S7-S11, P23 rewrites S12 — at which point this stage shrinks
    /// to a thin "wire it all together" call.
    fn stage_per_module_analysis(&mut self, hirs: Vec<(Uri, Hir, String, Duration)>) {
        for (uri, hir, lib, lower_took) in hirs {
            let mut timings = ModuleTimings {
                lower: lower_took,
                ..ModuleTimings::default()
            };
            let t0 = Instant::now();
            let resolutions = resolve_with_index(&hir, &self.index);
            timings.resolve = t0.elapsed();
            let t1 = Instant::now();
            let analysis =
                analyze_with_index_into(&hir, &resolutions, &self.index, &mut self.arena);
            timings.analyze = t1.elapsed();
            let t2 = Instant::now();
            let lints = run_lints(&hir, &resolutions);
            timings.lint = t2.elapsed();
            self.modules.insert(
                uri,
                ModuleAnalysis {
                    hir,
                    resolutions,
                    analysis,
                    lints,
                    lib,
                    timings,
                },
            );
        }
    }

    /// **Stage S12 cross-module suffix.** Remaining post-passes (P21
    /// deleted pass 3, P22+ will keep deleting):
    ///
    /// - Pass 3.4 (P16.3): cross-module member-expr typing.
    /// - Pass 3.5 (P15.7 + P16.4): cross-module call return-type
    ///   inference for Static / QualifiedStatic / Member / Arrow /
    ///   Ident callees.
    /// - Pass 3.52 (P16.4 follow-up): re-bind for-in iteration vars
    ///   from now-settled iterable types.
    /// - Fixed-point loop (P16.4 cascade closure): each pass propagates
    ///   type information one hop; bound at 5 iterations so a
    ///   degenerate/cyclic case can't hang.
    /// - Pass 3.55 (P16.6): typed lint — `arrow-on-non-deref`.
    /// - Pass 3.6: type-relation validation.
    ///
    /// **P22-P23 deletes most of these.** The work each pass does is
    /// subsumed by the staged S7-S11 (which lowers TypeRefs into the
    /// shared arena against the *complete* project name set, so
    /// cross-module attr / method types are resolved at signature
    /// time) and S12 (which walks bodies against fully-resolved
    /// signatures, so call-site monomorphization and member typing
    /// happen inline rather than in a fix-up sweep).
    fn stage_cross_module_post_passes(&mut self) {
        // P22 — passes 3.4 / 3.45 are gone: the analyzer's
        // Member/Arrow typing consults `index.type_members.attr_types`
        // inline via `Cx::foreign_member_type` during the body walk.
        //
        // Pass 3.5 (`infer_cross_module_call_types`) survives until P23
        // — it patches cross-module *call* return types, which the
        // analyzer's per-module body walker can't compute today
        // (foreign fn signatures aren't yet in `ProjectIndex`). When
        // P23 absorbs call typing into the staged S12, this falls.
        //
        // Pass 3.52 (`rebind_for_in_iter_types`) also survives until
        // P23 — it covers `for (g in foo())` where `foo()` returns a
        // foreign type and the analyzer's first pass typed the call
        // as `any`.
        let _ = self.infer_cross_module_call_types();
        self.rebind_for_in_iter_types();
        for _ in 0..3 {
            let prev_len = self
                .modules
                .values()
                .map(|m| m.analysis.expr_types.len())
                .sum::<usize>();
            let _ = self.infer_cross_module_call_types();
            self.rebind_for_in_iter_types();
            let new_len = self
                .modules
                .values()
                .map(|m| m.analysis.expr_types.len())
                .sum::<usize>();
            if new_len == prev_len {
                break;
            }
        }
        self.run_typed_lints(None);
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

    /// Pass 3.52 — after the cross-module type fixups have settled
    /// the iterable's type, re-derive the iteration variables' types.
    /// The analyzer's
    /// first pass binds them eagerly from `range_ty` at the visit-stmt
    /// site, but `range_ty` is `any` for foreign member-access / call
    /// iterables until 3.4 / 3.5 land. Mirrors the analyzer's
    /// generic-iterable unpacking so the binding logic stays in lockstep
    /// (`Array<T>` / `Set<T>` / `nodeList<T>` → `(int, T)`,
    /// `Map<K, V>` / `nodeIndex<K, V>` → `(K, V)`, `nodeTime<T>` →
    /// `(time, T)`, `nodeGeo<T>` → `(geo, T)`). Params with a declared
    /// type are not overridden.
    fn rebind_for_in_iter_types(&mut self) {
        use greycat_analyzer_hir::types::Stmt;
        use greycat_analyzer_types::{Primitive, TypeKind};

        type ParamUpdate = (Idx<greycat_analyzer_hir::types::Ident>, TypeShape);
        let mut updates: Vec<(Uri, Vec<ParamUpdate>)> = Vec::new();
        let arena = &self.arena;
        for (uri, m) in &self.modules {
            let mut entries: Vec<ParamUpdate> = Vec::new();
            for (_stmt_id, stmt) in m.hir.stmts.iter() {
                let Stmt::ForIn(f) = stmt else {
                    continue;
                };
                let Some(range_ty) = m.analysis.expr_types.get(&f.range).copied() else {
                    continue;
                };
                // Strip nullable so `for (i, v in arr?)` works the same
                // shape as the non-null case.
                let underlying = if arena.get(range_ty).nullable {
                    let mut t = arena.get(range_ty).clone();
                    t.nullable = false;
                    // Need a clone of the arena to mint into, because we
                    // can't borrow it mutably while iterating modules —
                    // we're already passing `TypeShape`s back through
                    // `mint_type_shape` so just translate the inner kind
                    // via `read_type_id_shape` below.
                    Some(t.kind)
                } else {
                    Some(arena.get(range_ty).kind.clone())
                };
                let Some(kind) = underlying else { continue };
                let int_shape = TypeShape::Primitive(Primitive::Int);
                let time_shape = TypeShape::Primitive(Primitive::Time);
                let geo_shape = TypeShape::Primitive(Primitive::Geo);
                let inferred: Vec<TypeShape> = match kind {
                    TypeKind::Generic { name, args }
                        if name == "Array" || name == "Set" || name == "nodeList" =>
                    {
                        let elem = args
                            .first()
                            .map(|id| read_type_id_shape(arena, *id))
                            .unwrap_or(TypeShape::Any);
                        if f.params.len() == 2 {
                            vec![int_shape, elem]
                        } else {
                            continue;
                        }
                    }
                    TypeKind::Generic { name, args } if name == "Map" || name == "nodeIndex" => {
                        if args.len() >= 2 && f.params.len() == 2 {
                            vec![
                                read_type_id_shape(arena, args[0]),
                                read_type_id_shape(arena, args[1]),
                            ]
                        } else {
                            continue;
                        }
                    }
                    TypeKind::Generic { name, args } if name == "nodeTime" => {
                        let elem = args
                            .first()
                            .map(|id| read_type_id_shape(arena, *id))
                            .unwrap_or(TypeShape::Any);
                        if f.params.len() == 2 {
                            vec![time_shape, elem]
                        } else {
                            continue;
                        }
                    }
                    TypeKind::Generic { name, args } if name == "nodeGeo" => {
                        let elem = args
                            .first()
                            .map(|id| read_type_id_shape(arena, *id))
                            .unwrap_or(TypeShape::Any);
                        if f.params.len() == 2 {
                            vec![geo_shape, elem]
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                };
                for (param, shape) in f.params.iter().zip(inferred) {
                    if param.ty.is_some() {
                        // Declared type wins.
                        continue;
                    }
                    entries.push((param.name, shape));
                }
            }
            if !entries.is_empty() {
                updates.push((uri.clone(), entries));
            }
        }
        let arena_mut = &mut self.arena;
        for (uri, entries) in updates {
            let Some(m) = self.modules.get_mut(&uri) else {
                continue;
            };
            // Mint each shape once, then write `def_types` AND every
            // `Expr::Ident` use that resolves to that binding through
            // `expr_types` so 3.6's call-arg validation sees the
            // settled type at boundaries. Member-of / call-of chains
            // off the rebound binding stay `any` for now (pass 2's
            // bottom-up evaluation already typed them); closing that
            // cascade is a follow-up.
            let mut name_to_ty: HashMap<Idx<greycat_analyzer_hir::types::Ident>, _> =
                HashMap::new();
            for (name_idx, shape) in entries {
                let ty = mint_type_shape(&shape, arena_mut);
                m.analysis.def_types.insert(name_idx, ty);
                name_to_ty.insert(name_idx, ty);
            }
            // Ident-use expr_types fixup. Walks every Expr::Ident in
            // the module and overwrites entries that resolve to a
            // freshly-rebound for-in iter param.
            use crate::resolver::Definition;
            use greycat_analyzer_hir::types::Expr;
            for (expr_id, expr) in m.hir.exprs.iter() {
                let Expr::Ident(use_idx) = expr else {
                    continue;
                };
                let Some(def) = m.resolutions.lookup(*use_idx) else {
                    continue;
                };
                let target_name = match def {
                    Definition::Param(n) | Definition::Local(n) => n,
                    _ => continue,
                };
                let Some(ty) = name_to_ty.get(&target_name).copied() else {
                    continue;
                };
                m.analysis.expr_types.insert(expr_id, ty);
            }
        }
    }

    /// P15.7 — cross-module call return-type inference. After the
    /// analyzer's per-module pass has populated `foreign_member_uses`
    /// (via P21's structure index), walk every module's HIR `Expr::Call`s
    /// whose callee is `Expr::Static` and whose property binds to a
    /// foreign `Method`. Look up the foreign method's declared
    /// return type, translate it into the *current module's*
    /// `analysis.types` arena, and overwrite the placeholder `any`
    /// in `analysis.expr_types[call_id]` so inlay hints / hover /
    /// downstream inference see the right type.
    ///
    /// Generic substitution across modules is deferred: when the
    /// foreign method's return type depends on a generic, this pass
    /// keeps the generic shape (e.g. `Array<T>`) without binding `T`.
    /// Concrete returns (`Identity`, `String`, `Array<Permission>`)
    /// flow through cleanly.
    fn infer_cross_module_call_types(&mut self) -> HashSet<String> {
        use crate::analyzer::{ForeignDecl, ForeignMember};
        use greycat_analyzer_hir::types::{Expr, Stmt};

        let mut touched_uris: HashSet<String> = HashSet::new();
        // Phase 1 — read-only: collect the type-shape that each
        // affected expr should carry, plus a list of Stmt::Var whose
        // init expr feeds into one of those updates so we can re-link
        // their `def_types` afterwards.
        #[allow(clippy::mutable_key_type)]
        let mut expr_updates: HashMap<Uri, Vec<(Idx<Expr>, TypeShape)>> = HashMap::new();
        // P15.x — chain-segment bindings collected during the same
        // QualifiedStatic walk. chain[1] (the type segment) lands in
        // `chain_type_updates`; chain[2] (the member segment) lands
        // in `chain_member_updates`. Phase 2 writes them into
        // `analysis.foreign_decl_uses` / `foreign_member_uses` so
        // hover / goto-def see the right foreign target on every
        // segment.
        #[allow(clippy::mutable_key_type)]
        let mut chain_type_updates: HashMap<
            Uri,
            Vec<(Idx<greycat_analyzer_hir::types::Ident>, ForeignDecl)>,
        > = HashMap::new();
        #[allow(clippy::mutable_key_type)]
        let mut chain_member_updates: HashMap<
            Uri,
            Vec<(Idx<greycat_analyzer_hir::types::Ident>, ForeignMember)>,
        > = HashMap::new();
        // For each module, find every Expr::Static and decide what
        // its expr_type should be (method-ref → function,
        // attr-ref → field, etc.). Then for any Expr::Call whose
        // callee is one of those Static exprs, override with the
        // method's return type.
        for (cur_uri, cur_module) in &self.modules {
            // 1a) Static-expr standalone shapes (`Type::create`, `Type::id`).
            for (static_id, static_expr) in cur_module.hir.exprs.iter() {
                let Expr::Static(s) = static_expr else {
                    continue;
                };
                let shape =
                    match resolve_static_member_shape(&self.modules, &self.index, cur_module, s) {
                        Some(sh) => sh,
                        None => continue,
                    };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((static_id, shape));
            }
            // 1a-tris) Bare ident references to a top-level decl
            // (`Identity`, `someFn` used as a value). Type decls
            // become `type`, fn decls become `function`. Both
            // intra- and cross-module shapes are covered.
            for (ident_expr_id, ident_expr) in cur_module.hir.exprs.iter() {
                let Expr::Ident(name_idx) = ident_expr else {
                    continue;
                };
                let shape =
                    match resolve_bare_ident_decl_shape(&self.modules, cur_module, *name_idx) {
                        Some(sh) => sh,
                        None => continue,
                    };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((ident_expr_id, shape));
            }
            // 1a-bis) QualifiedStatic standalone shapes (P15.8 chained).
            for (qstatic_id, qstatic_expr) in cur_module.hir.exprs.iter() {
                let Expr::QualifiedStatic { chain, .. } = qstatic_expr else {
                    continue;
                };
                let Some(shape) =
                    resolve_qualified_static_shape(&self.modules, &self.index, cur_module, chain)
                else {
                    continue;
                };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((qstatic_id, shape));
                // Bind chain[1] (the type segment) and chain[2] (the
                // member segment) to their foreign decls so hover /
                // goto-def can render the right thing on each part.
                if let Some((module_uri, type_decl_id, target)) =
                    resolve_qualified_chain(&self.modules, &self.index, cur_module, chain)
                {
                    chain_type_updates
                        .entry(cur_uri.clone())
                        .or_default()
                        .push((
                            chain[1],
                            ForeignDecl {
                                uri: module_uri.clone(),
                                decl: type_decl_id,
                            },
                        ));
                    if let QualifiedTarget::Member(member) = target {
                        chain_member_updates
                            .entry(cur_uri.clone())
                            .or_default()
                            .push((
                                chain[2],
                                ForeignMember {
                                    uri: module_uri,
                                    member,
                                },
                            ));
                    }
                    // EnumVariant binding intentionally has no
                    // `foreign_member_uses` entry — variants aren't
                    // attrs / methods and the analyzer doesn't track
                    // them in `member_uses`. Hover / goto-def for the
                    // last segment will fall back to the chain[1]
                    // type-decl binding (already populated above).
                }
            }
            // 1b) Call(Static / QualifiedStatic / Member / Arrow / Ident)
            // — overrides the post-analysis expr_type with the call's
            // declared return-type. The analyzer's first pass returns
            // `any` for every Call (modulo generic constraint solving),
            // so this post-pass is the *only* place a call gets its
            // proper return type. Every callee shape that resolves to
            // a `Decl::Fn` must be covered here — otherwise inlay
            // hints, hover, var-init typing, etc. all fall back to
            // `any` for that shape.
            //
            // P16.4 added Member / Arrow. The Ident arm covers bare
            // same-module / cross-module fn-decl calls (`foo()` /
            // `module::foo()` shapes routed through `Definition::Decl`
            // and `Definition::ProjectDecl`).
            for (call_id, call_expr) in cur_module.hir.exprs.iter() {
                let Expr::Call(call) = call_expr else {
                    continue;
                };
                let callee_expr = &cur_module.hir.exprs[call.callee];
                let shape = match callee_expr {
                    Expr::Static(s) => {
                        resolve_static_call_return_shape(&self.modules, cur_module, s)
                    }
                    Expr::QualifiedStatic { chain, .. } => resolve_qualified_static_call_shape(
                        &self.modules,
                        &self.index,
                        cur_module,
                        chain,
                    ),
                    Expr::Member(m) | Expr::Arrow(m) => {
                        // P16.4 — substitute generic params on the
                        // method's return type using the receiver's
                        // instantiation. `nodeIndex<K, V>::get(K) → V?`
                        // with `pkgs: nodeIndex<String, node<Pkg>>`
                        // produces `node<Pkg>?` instead of `V?`.
                        let recv_ty = cur_module.analysis.expr_types.get(&m.receiver).copied();
                        resolve_member_call_return_shape_subst(
                            &self.modules,
                            &self.index,
                            &self.arena,
                            cur_module,
                            m.property,
                            recv_ty,
                            matches!(callee_expr, Expr::Arrow(_)),
                        )
                    }
                    Expr::Ident(_) => resolve_ident_call_return_shape(
                        &self.modules,
                        &self.index,
                        cur_module,
                        call.callee,
                    ),
                    _ => None,
                };
                let Some(shape) = shape else {
                    continue;
                };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((call_id, shape));
            }
        }

        // Phase 2 — mutable: mint the snapshotted shapes into the
        // shared project arena (P19) and update `expr_types`. Then
        // walk `Stmt::Var` to re-link `def_types` for locals whose
        // init expr we just updated.
        let arena_mut = &mut self.arena;
        for (uri, entries) in expr_updates {
            let Some(m) = self.modules.get_mut(&uri) else {
                continue;
            };
            touched_uris.insert(uri.as_str().to_string());
            // Build a small index of which exprs we touched.
            let mut touched: HashMap<Idx<Expr>, greycat_analyzer_types::TypeId> = HashMap::new();
            for (expr_id, shape) in entries {
                let ty = mint_type_shape(&shape, arena_mut);
                m.analysis.expr_types.insert(expr_id, ty);
                touched.insert(expr_id, ty);
            }
            // Re-link `def_types` for `var x = <touched_expr>;` shapes.
            // The analyzer's first pass set `def_types[name] = init_ty`
            // where `init_ty` was the placeholder `any` for these.
            for (_stmt_id, stmt) in m.hir.stmts.iter() {
                let Stmt::Var(local) = stmt else {
                    continue;
                };
                let Some(init) = local.init else {
                    continue;
                };
                if local.ty.is_some() {
                    // Declared type wins; the analyzer already stored it.
                    continue;
                }
                let Some(new_ty) = touched.get(&init).copied() else {
                    continue;
                };
                m.analysis.def_types.insert(local.name, new_ty);
            }
        }

        // Phase 2 (continued) — write chain-segment bindings.
        for (uri, entries) in chain_type_updates {
            if let Some(m) = self.modules.get_mut(&uri) {
                for (ident_idx, fd) in entries {
                    m.analysis.foreign_decl_uses.insert(ident_idx, fd);
                }
            }
        }
        for (uri, entries) in chain_member_updates {
            if let Some(m) = self.modules.get_mut(&uri) {
                for (ident_idx, fm) in entries {
                    m.analysis.foreign_member_uses.insert(ident_idx, fm);
                }
            }
        }
        touched_uris
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
    /// P19 split-borrow variant: takes `&modules`, `&index`, and a
    /// mutable borrow on the shared arena. The validation loop holds
    /// `&mut self.arena` during iteration over `&self.modules`, so the
    /// `&self`-borrowing version can no longer be invoked directly
    /// from the same scope.
    #[allow(clippy::mutable_key_type)]
    fn collect_call_arg_diags_split(
        modules: &HashMap<Uri, ModuleAnalysis>,
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
            if !fnd.generics.is_empty() {
                continue;
            }
            let pair_count = fnd.params.len().min(call.args.len());
            for i in 0..pair_count {
                let p = &fn_module.hir.fn_params[fnd.params[i]];
                let Some(declared_ref) = p.ty else {
                    continue;
                };
                let declared_shape = read_type_shape(&fn_module.hir, declared_ref);
                let arg_ty = match cur_module.analysis.expr_types.get(&call.args[i]).copied() {
                    Some(t) => t,
                    None => continue,
                };
                // P19: mint declared types directly into the shared
                // project arena. Append-only + interned, so re-mints
                // collapse and `arg_ty` (already from this arena) is
                // comparable head-on.
                let declared_ty = mint_type_shape(&declared_shape, arena);
                if !greycat_analyzer_types::is_assignable_to(arena, arg_ty, declared_ty) {
                    let p_name = fn_module.hir.idents[p.name].text.clone();
                    let arg_display = greycat_analyzer_types::display(arena, arg_ty);
                    let declared_display = greycat_analyzer_types::display(arena, declared_ty);
                    let r = match &cur_module.hir.exprs[call.args[i]] {
                        Expr::Ident(idx) => cur_module.hir.idents[*idx].byte_range.clone(),
                        other => other.byte_range(),
                    };
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
    /// - `restrict = Some(set)` only revalidates the listed URIs —
    ///   the changed URI plus any module whose `expr_types` were
    ///   touched by the cross-module fixup passes. Used by
    ///   `invalidate` to keep per-keystroke cost bounded.
    ///
    /// P16.6 — typed lints that depend on settled per-expr types and
    /// the project-wide [`ProjectIndex`]. Runs after the cross-module
    /// type fixup passes (3.4 / 3.5) and before
    /// [`Self::validate_type_relations`]. Idempotent: drops prior
    /// findings for the rule before re-emitting.
    ///
    /// `restrict = None` lints every cached module; `Some(set)` only
    /// the listed URIs (matches the type-relation validation scope).
    fn run_typed_lints(&mut self, restrict: Option<&HashSet<String>>) {
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
        for (uri, module) in self.modules.iter_mut() {
            if !in_scope(uri) {
                continue;
            }
            module.lints.retain(|l| l.rule != "arrow-on-non-deref");
            lint_arrow_on_non_deref(
                &module.hir,
                &module.analysis,
                arena,
                index,
                &mut module.lints,
            );
        }
    }

    fn validate_type_relations(&mut self, restrict: Option<&HashSet<String>>) {
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
        let mut diag_updates: HashMap<Uri, Vec<SemanticDiagnostic>> = HashMap::new();
        // P19 — split borrows: pass the shared arena alongside read-only
        // module borrows.
        let arena_mut = &mut self.arena;
        for (cur_uri, cur_module) in &self.modules {
            if !in_scope(cur_uri) {
                continue;
            }
            let mut diags: Vec<SemanticDiagnostic> = Vec::new();
            validate_module_type_relations(cur_module, arena_mut, &mut diags);
            // Call-arg validation needs cross-module access (foreign
            // fn signatures), so it lives on `&self` rather than the
            // free walker. Note: we hold `arena_mut` here, so call into
            // a helper that accepts `&self.modules` + `&self.index` +
            // `arena` instead of borrowing `&self`.
            diags.extend(Self::collect_call_arg_diags_split(
                &self.modules,
                &self.index,
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
    fn assert_no_in_scope_type_relation_diags(&self, restrict: Option<&HashSet<String>>) {
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

    /// P14.9 — walk every module's CST for qualified-name access
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
        let mut by_name: HashMap<String, _Uri> = HashMap::new();
        for (uri, cell) in manager.iter() {
            let doc = cell.borrow();
            by_name.insert(doc.name().to_string(), uri.clone());
        }

        // 2. Walk every module's CST for `static_expr` nodes whose
        // chain root names a known module. Collect bumps.
        #[allow(clippy::mutable_key_type)]
        let mut bumps: HashMap<_Uri, Vec<Idx<Decl>>> = HashMap::new();
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
            crate::lint::LintRule::check(
                &crate::lint::UnusedDecl,
                &module.hir,
                &module.resolutions,
                &mut new_lints,
            );
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
        // Drop cache entries for URIs no longer in the manager. `Uri`
        // has interior mutability for LSP wire-form caching, so we key
        // the live set by string instead of stuffing it into a HashSet.
        let live: HashSet<String> = manager
            .iter()
            .map(|(u, _)| u.as_str().to_string())
            .collect();
        self.modules.retain(|u, _| live.contains(u.as_str()));

        // Lower the changed doc fresh; reuse cached HIRs for the rest.
        let mut lower_took = Duration::ZERO;
        let mut changed_lib: Option<String> = None;
        let changed_hir = manager.get(uri).map(|cell| {
            let doc = cell.borrow();
            let start = Instant::now();
            let hir = lower_module(&doc.text, "module", &doc.lib, doc.root_node());
            lower_took = start.elapsed();
            changed_lib = Some(doc.lib.clone());
            hir
        });

        // Rebuild the shared index. ingest is name-additive (idempotent
        // on repeated calls with the same module), so starting from a
        // fresh index is what makes deletions visible.
        let mut new_index = ProjectIndex::new();
        if let Some(hir) = &changed_hir {
            new_index.ingest(uri, hir);
        }
        for (other_uri, ma) in &self.modules {
            if other_uri == uri {
                continue;
            }
            new_index.ingest(other_uri, &ma.hir);
        }
        // For docs that are in the manager but not yet in the cache
        // (e.g. freshly added, never analyzed), lower them so the index
        // sees their decls. Their per-module analysis runs only on
        // their own invalidate call.
        for (other_uri, cell) in manager.iter() {
            if other_uri == uri || self.modules.contains_key(other_uri) {
                continue;
            }
            let doc = cell.borrow();
            let hir = lower_module(&doc.text, "module", &doc.lib, doc.root_node());
            new_index.ingest(other_uri, &hir);
        }
        self.index = new_index;

        let Some(hir) = changed_hir else {
            // `uri` has been removed — drop any stale entry.
            self.modules.remove(uri);
            return;
        };
        let mut timings = ModuleTimings {
            lower: lower_took,
            ..ModuleTimings::default()
        };
        let t0 = Instant::now();
        let resolutions = resolve_with_index(&hir, &self.index);
        timings.resolve = t0.elapsed();
        let t1 = Instant::now();
        let analysis = analyze_with_index_into(&hir, &resolutions, &self.index, &mut self.arena);
        timings.analyze = t1.elapsed();
        let t2 = Instant::now();
        let lints = run_lints(&hir, &resolutions);
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
            },
        );
        // P21 — pass 3 (`resolve_cross_module_members`) is gone; the
        // analyzer's `resolve_member` now consults the project-wide
        // `type_members` index directly during the per-module body
        // walk, so cross-module bindings land in `foreign_member_uses`
        // inline.
        // P22 — cross-module member typing happens inline in the
        // analyzer (via `Cx::foreign_member_type`), so no pass 3.4
        // call here. Pass 3.5 (call returns) and pass 3.52 (for-in
        // rebind) survive until P23.
        let mut touched: HashSet<String> = HashSet::new();
        touched.insert(uri.as_str().to_string()); // changed doc itself
        touched.extend(self.infer_cross_module_call_types());
        self.rebind_for_in_iter_types();
        // Typed lint pass (P16.6). Same scope as `validate_type_relations`
        // — only the modules whose types changed need re-linting.
        self.run_typed_lints(Some(&touched));
        // Incremental validation — only the changed URI plus any
        // module whose types were updated by the post-passes.
        self.validate_type_relations(Some(&touched));
        self.compute_qualified_refs(manager);
    }

    pub fn module(&self, uri: &Uri) -> Option<&ModuleAnalysis> {
        self.modules.get(uri)
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

/// P14.9 — pull every ident text from a `static_expr` chain (left to
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

/// P15.x — bare-ident decl reference. `Identity` (without
/// `::name`) used as a value evaluates to a `type` runtime
/// reflection value; `someFn` evaluates to a `function`. Returns
/// `None` for idents that aren't decl-bound (locals / params /
/// generics / unresolved).
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_bare_ident_decl_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    cur: &ModuleAnalysis,
    ident_idx: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<TypeShape> {
    use crate::resolver::Definition;
    let def = cur.resolutions.lookup(ident_idx)?;
    let (host_hir, decl_id) = match def {
        Definition::Decl(decl_id) => (&cur.hir, decl_id),
        Definition::ProjectDecl { uri, decl } => {
            let m = modules.get(&uri)?;
            (&m.hir, decl)
        }
        _ => return None,
    };
    Some(match &host_hir.decls[decl_id] {
        Decl::Type(_) | Decl::Enum(_) => TypeShape::Named {
            name: "type".to_string(),
            params: Vec::new(),
        },
        Decl::Fn(_) => TypeShape::Named {
            name: "function".to_string(),
            params: Vec::new(),
        },
        Decl::Var(vd) => match vd.ty {
            Some(t) => read_type_shape(host_hir, t),
            None => return None,
        },
        Decl::Pragma(_) => return None,
    })
}

/// P15.10 — resolve a call's callee to its declaring `Decl::Fn`.
/// Returns `(Some(foreign_uri), decl_id)` for cross-module callees
/// and `(None, decl_id)` for in-module callees.
///
/// Covers four callee shapes:
///   * `Expr::Ident` -> `Definition::Decl(Decl::Fn)` (in-module top-level).
///   * `Expr::Ident` -> `Definition::ProjectDecl { uri, decl }` where decl is `Decl::Fn`.
///   * `Expr::Static` -> `member_uses` -> `MemberDef::Method(decl_id)` (intra-module).
///   * `Expr::Static` -> `foreign_member_uses` -> `MemberDef::Method(decl_id)` (cross-module).
///   * `Expr::QualifiedStatic` -> `resolve_qualified_chain` -> `MemberDef::Method`.
///
/// Other shapes (`Expr::Member` / lambda / etc.) return `None`.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_call_target(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    callee: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
) -> Option<(Option<Uri>, Idx<Decl>)> {
    use crate::analyzer::MemberDef;
    use crate::resolver::Definition;
    use greycat_analyzer_hir::types::Expr;

    let callee_expr = &cur.hir.exprs[callee];
    match callee_expr {
        Expr::Ident(name_idx) => match cur.resolutions.lookup(*name_idx)? {
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
        Expr::Static(s) => {
            if let Some(MemberDef::Method(decl_id)) = cur.analysis.member_lookup(s.property) {
                return Some((None, decl_id));
            }
            if let Some(foreign) = cur.analysis.foreign_member_lookup(s.property)
                && let MemberDef::Method(decl_id) = foreign.member
            {
                return Some((Some(foreign.uri.clone()), decl_id));
            }
            None
        }
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

/// P15.7 — figure out what type a standalone `Expr::Static`
/// (`Type::name` *not* immediately followed by a call) should carry.
/// Method references are `function`; attr references are `field`. This
/// matches GreyCat's reflection model where `Type::method` yields a
/// callable function value and `Type::attr` yields a field handle.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_static_member_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    s: &greycat_analyzer_hir::types::StaticExpr,
) -> Option<TypeShape> {
    if let Some(local) = cur.analysis.member_lookup(s.property) {
        return Some(static_shape_from_member(&local));
    }
    if let Some(foreign) = cur.analysis.foreign_member_lookup(s.property) {
        // Sanity check the foreign module is still cached.
        if modules.contains_key(&foreign.uri) {
            return Some(static_shape_from_member(&foreign.member));
        }
    }
    // P15.x — `module::Decl` shape. Read the static_expr's TypeRef
    // text; if it matches a known module name, look up the property
    // as a top-level decl in that module's HIR. Type/enum decls
    // become `type` (the runtime native), fn decls become `function`,
    // and var decls inherit their declared type.
    let ty_name = cur.hir.idents[cur.hir.type_refs[s.ty].name].text.as_str();
    let module_uri = index.module_uri(ty_name)?;
    let module = modules.get(module_uri)?;
    let property_text = cur.hir.idents[s.property].text.as_str();
    let module_root = module.hir.module.as_ref()?;
    for decl_id in &module_root.decls {
        let decl = &module.hir.decls[*decl_id];
        let Some(name_id) = decl.name() else {
            continue;
        };
        if module.hir.idents[name_id].text != property_text {
            continue;
        }
        return Some(match decl {
            Decl::Type(_) | Decl::Enum(_) => TypeShape::Named {
                name: "type".to_string(),
                params: Vec::new(),
            },
            Decl::Fn(_) => TypeShape::Named {
                name: "function".to_string(),
                params: Vec::new(),
            },
            Decl::Var(vd) => match vd.ty {
                Some(t) => read_type_shape(&module.hir, t),
                None => TypeShape::Any,
            },
            Decl::Pragma(_) => return None,
        });
    }
    None
}

fn static_shape_from_member(member: &crate::analyzer::MemberDef) -> TypeShape {
    use crate::analyzer::MemberDef;
    match member {
        MemberDef::Method(_) => TypeShape::Named {
            name: "function".to_string(),
            params: Vec::new(),
        },
        MemberDef::Attr(_) => TypeShape::Named {
            name: "field".to_string(),
            params: Vec::new(),
        },
    }
}

/// P15.8 — resolve a 3-segment `module::Type::member` chain to its
/// foreign decl shape. Returns the static-expr-as-value type:
/// methods → `function`, attrs → `field`, types → `type`.
/// Returns `None` for chains that don't match a known module / type
/// / member, or for chains of length other than 3 (length 2 is
/// already handled by `Expr::Static`; longer chains aren't supported
/// yet).
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_qualified_static_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    chain: &[Idx<greycat_analyzer_hir::types::Ident>],
) -> Option<TypeShape> {
    let (module_uri, _type_decl_id, foreign) = resolve_qualified_chain(modules, index, cur, chain)?;
    use crate::analyzer::MemberDef;
    Some(match foreign {
        QualifiedTarget::Member(MemberDef::Method(_)) => TypeShape::Named {
            name: "function".to_string(),
            params: Vec::new(),
        },
        QualifiedTarget::Member(MemberDef::Attr(_)) => TypeShape::Named {
            name: "field".to_string(),
            params: Vec::new(),
        },
        // Enum-variant access — the value's type is the enum itself,
        // not `field` / `function`. Mirrors the in-module
        // `Expr::Static` handling for `Foo::a`.
        QualifiedTarget::EnumVariant { decl } => {
            let foreign_module = modules.get(&module_uri)?;
            let Decl::Enum(ed) = &foreign_module.hir.decls[decl] else {
                return None;
            };
            TypeShape::Named {
                name: foreign_module.hir.idents[ed.name].text.clone(),
                params: Vec::new(),
            }
        }
    })
}

/// P15.8 — return the call return type for `module::Type::method(...)`.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_qualified_static_call_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    chain: &[Idx<greycat_analyzer_hir::types::Ident>],
) -> Option<TypeShape> {
    use crate::analyzer::MemberDef;
    let (module_uri, _type_decl_id, target) = resolve_qualified_chain(modules, index, cur, chain)?;
    let QualifiedTarget::Member(MemberDef::Method(decl_id)) = target else {
        return None;
    };
    let foreign_module = modules.get(&module_uri)?;
    let Decl::Fn(fnd) = &foreign_module.hir.decls[decl_id] else {
        return None;
    };
    let ret = fnd.return_type?;
    Some(read_type_shape(&foreign_module.hir, ret))
}

enum QualifiedTarget {
    Member(crate::analyzer::MemberDef),
    /// Enum-variant access — `module::Foo::a` where `Foo` is an
    /// enum decl and `a` matches one of its variants. Carries the
    /// enum decl so callers can mint the enum's TypeShape.
    EnumVariant {
        decl: Idx<Decl>,
    },
}

/// Walk a `module::Type::member` chain and resolve each segment.
/// Returns (foreign_module_uri, type_decl_id, target). Length
/// must be exactly 3; other lengths return `None`.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_qualified_chain(
    modules: &HashMap<Uri, ModuleAnalysis>,
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
                    return Some((
                        module_uri,
                        type_decl_id,
                        QualifiedTarget::EnumVariant { decl: type_decl_id },
                    ));
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

/// P15.7 — figure out what return type a `Call(callee=Static)` should
/// carry. For Method bindings, that's the foreign / in-module method's
/// declared `return_type`. Returns `None` for attr bindings (calling
/// a `field` is nonsense) or unbound statics.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_static_call_return_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    cur: &ModuleAnalysis,
    s: &greycat_analyzer_hir::types::StaticExpr,
) -> Option<TypeShape> {
    use crate::analyzer::MemberDef;
    if let Some(MemberDef::Method(decl_id)) = cur.analysis.member_lookup(s.property) {
        let Decl::Fn(fnd) = &cur.hir.decls[decl_id] else {
            return None;
        };
        let ret = fnd.return_type?;
        return Some(read_type_shape(&cur.hir, ret));
    }
    if let Some(foreign) = cur.analysis.foreign_member_lookup(s.property)
        && let MemberDef::Method(decl_id) = foreign.member
        && let Some(foreign_module) = modules.get(&foreign.uri)
        && let Decl::Fn(fnd) = &foreign_module.hir.decls[decl_id]
        && let Some(ret) = fnd.return_type
    {
        return Some(read_type_shape(&foreign_module.hir, ret));
    }
    None
}

/// Return-type for a bare-ident fn call (`foo()` / `module::foo()`
/// where the callee resolves to a `Decl::Fn`). Reuses
/// [`resolve_call_target`] which already knows how to walk the
/// resolver tables. Returns `None` for callees that aren't fn decls
/// (calling a local / param / type — those produce `any` anyway in
/// the first pass) or for fn decls without a declared return type.
///
/// This closes the architectural gap that surfaced as
/// `var s = foo()` typing as `any` even when `foo: () -> String?` was
/// declared in the same module — the analyzer's first-pass
/// `Expr::Call` arm short-circuits to `any` for non-generic Ident
/// callees because resolving the callee needs the full
/// `ProjectAnalysis` context, which doesn't exist yet inside the
/// per-module analyzer.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_ident_call_return_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    cur: &ModuleAnalysis,
    callee: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::Expr>,
) -> Option<TypeShape> {
    let (foreign_uri, decl_id) = resolve_call_target(modules, index, cur, callee)?;
    let host_hir = match foreign_uri {
        Some(uri) => &modules.get(&uri)?.hir,
        None => &cur.hir,
    };
    let Decl::Fn(fnd) = &host_hir.decls[decl_id] else {
        return None;
    };
    let ret = fnd.return_type?;
    Some(read_type_shape(host_hir, ret))
}

/// P16.4 — return-type for `recv.method(args)` / `recv->method(args)`.
/// Looks up the property's binding (intra-module via `member_uses`,
/// cross-module via `foreign_member_uses`) and reads the bound
/// method's declared return type. Returns `None` for attr bindings
/// (calling a field is nonsense), un-bound properties, or methods
/// without a declared return type.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_member_call_return_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
    cur: &ModuleAnalysis,
    property_idx: Idx<greycat_analyzer_hir::types::Ident>,
) -> Option<TypeShape> {
    use crate::analyzer::MemberDef;
    if let Some(MemberDef::Method(decl_id)) = cur.analysis.member_lookup(property_idx) {
        let Decl::Fn(fnd) = &cur.hir.decls[decl_id] else {
            return None;
        };
        let ret = fnd.return_type?;
        return Some(read_type_shape(&cur.hir, ret));
    }
    if let Some(foreign) = cur.analysis.foreign_member_lookup(property_idx)
        && let MemberDef::Method(decl_id) = foreign.member
        && let Some(foreign_module) = modules.get(&foreign.uri)
        && let Decl::Fn(fnd) = &foreign_module.hir.decls[decl_id]
        && let Some(ret) = fnd.return_type
    {
        return Some(read_type_shape(&foreign_module.hir, ret));
    }
    None
}

/// P16.4 — substitution-aware companion to `resolve_member_call_return_shape`.
/// When the receiver's TypeKind exposes a generic instantiation
/// (`nodeIndex<String, node<Pkg>>`), look up the method's host
/// `TypeDecl` to pair its `generics` (e.g. `["K", "V"]`) with the
/// instantiation args, build a substitution map, and read the return
/// type's `TypeRef` through `read_type_shape_subst`. For arrow access
/// (`n->m()`) the receiver is auto-derefed through node-tag generics
/// (`node<T>` → `T`) before the lookup.
///
/// Falls back to plain `read_type_shape` (no substitution) when the
/// receiver's TypeKind doesn't carry generic args, when the method's
/// host type isn't reachable, or when the property isn't bound to a
/// method in the per-module / cross-module member maps.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_member_call_return_shape_subst(
    modules: &HashMap<Uri, ModuleAnalysis>,
    index: &ProjectIndex,
    arena: &greycat_analyzer_types::TypeArena,
    cur: &ModuleAnalysis,
    property_idx: Idx<greycat_analyzer_hir::types::Ident>,
    receiver_ty: Option<greycat_analyzer_types::TypeId>,
    is_arrow: bool,
) -> Option<TypeShape> {
    use crate::analyzer::MemberDef;
    use greycat_analyzer_types::TypeKind;

    // Resolve the property's host module + method decl. The two existing
    // member-lookup tables (intra-module `member_uses` and cross-module
    // `foreign_member_uses`) already point at the right `Decl::Fn`; we
    // reuse them so the substitution path doesn't have to redo the
    // member-resolution from scratch.
    let (host_hir, decl_id): (&Hir, Idx<Decl>) =
        if let Some(MemberDef::Method(decl_id)) = cur.analysis.member_lookup(property_idx) {
            (&cur.hir, decl_id)
        } else if let Some(foreign) = cur.analysis.foreign_member_lookup(property_idx)
            && let MemberDef::Method(decl_id) = foreign.member
            && let Some(foreign_module) = modules.get(&foreign.uri)
        {
            (&foreign_module.hir, decl_id)
        } else {
            // Fall back to plain shape resolution when no binding has
            // been recorded yet — the loop that calls us will see the
            // result update on a later iteration once member resolution
            // catches up.
            return resolve_member_call_return_shape(modules, cur, property_idx);
        };
    let Decl::Fn(fnd) = &host_hir.decls[decl_id] else {
        return None;
    };
    let ret = fnd.return_type?;

    // Build the substitution map from the receiver's generic args.
    // We need (a) the type decl whose generics name K, V, ... and
    // (b) the receiver's instantiation supplying their values. For
    // `recv->method()` we auto-deref through node-tag generics so
    // `n->m()` on `n: node<T>` substitutes against `T`'s generics.
    let mut subst: HashMap<String, TypeShape> = HashMap::new();
    if let Some(recv_ty) = receiver_ty {
        let lookup_ty = if is_arrow {
            match arena.get(recv_ty).kind.clone() {
                TypeKind::Generic { args, name }
                    if greycat_analyzer_types::is_node_tag(&name) && args.len() == 1 =>
                {
                    args[0]
                }
                _ => recv_ty,
            }
        } else {
            recv_ty
        };
        let (type_name, instantiation) = match arena.get(lookup_ty).kind.clone() {
            TypeKind::Named { name } => (name, Vec::new()),
            TypeKind::Generic { name, args } => (name, args),
            TypeKind::Primitive(p) => (p.name().to_string(), Vec::new()),
            _ => (String::new(), Vec::new()),
        };
        // Look up the type decl: own module first, then cross-module
        // via the project index. The lookup is for *the receiver's
        // type*, which carries the generic params we need to match.
        // Note this can be a different module than `host_hir` (the
        // module owning the method's `Decl::Fn`) when a generic type
        // is declared in one module and the method is in the same
        // module. For native generic types like `nodeIndex` both live
        // together in stdlib.
        let lookup: Option<(&Hir, Idx<Decl>)> =
            if let Some(decl_id) = cur.analysis.type_decls.get(&type_name).copied() {
                Some((&cur.hir, decl_id))
            } else {
                index
                    .locate_decl(&type_name)
                    .first()
                    .and_then(|(uri, decl_id)| modules.get(uri).map(|m| (&m.hir, *decl_id)))
            };
        if let Some((td_hir, td_decl_id)) = lookup
            && let Decl::Type(td) = &td_hir.decls[td_decl_id]
        {
            for (gp_idx, name_idx) in td.generics.iter().enumerate() {
                let gp_name = td_hir.idents[*name_idx].text.clone();
                let arg_shape = instantiation
                    .get(gp_idx)
                    .map(|a| read_type_id_shape(arena, *a))
                    .unwrap_or(TypeShape::Any);
                subst.insert(gp_name, arg_shape);
            }
        }
    }
    Some(read_type_shape_subst(host_hir, ret, &subst))
}

/// Walk one module's HIR and emit every type-relation diagnostic
/// the analyzer's per-module pass deferred. Reads only — never
/// mutates `module`. The shared project arena is passed in (P19);
/// any newly-needed declared-side TypeIds are minted into it
/// alongside everything else, which is fine because the arena is
/// append-only and intern-collapsed.
fn validate_module_type_relations(
    module: &ModuleAnalysis,
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
        validate_decl(hir, analysis, arena, bool_t, &hir.decls[*d_id], diags);
    }

    fn validate_decl(
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
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
                    validate_stmt(hir, analysis, arena, bool_t, body, return_ty, diags);
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
                    validate_decl(hir, analysis, arena, bool_t, &hir.decls[*m], diags);
                }
            }
            Decl::Var(vd) => {
                if let (Some(decl_ref), Some(init)) = (vd.ty, vd.init)
                    && let Some(declared_ty) =
                        lower_type_ref_id(hir, decl_ref, &analysis.registry, arena)
                {
                    check_assign(
                        analysis,
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

    fn validate_block(
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
        arena: &mut greycat_analyzer_types::TypeArena,
        bool_t: greycat_analyzer_types::TypeId,
        block: &greycat_analyzer_hir::types::BlockStmt,
        return_ty: Option<greycat_analyzer_types::TypeId>,
        diags: &mut Vec<SemanticDiagnostic>,
    ) {
        for s in &block.stmts {
            validate_stmt(hir, analysis, arena, bool_t, *s, return_ty, diags);
        }
    }

    fn validate_stmt(
        hir: &greycat_analyzer_hir::Hir,
        analysis: &crate::analyzer::AnalysisResult,
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
            Stmt::Block(b) => validate_block(hir, analysis, arena, bool_t, b, return_ty, diags),
            Stmt::Var(LocalVar { ty, init, .. }) => {
                if let (Some(decl_ref), Some(init_id)) = (ty, init)
                    && let Some(declared_ty) =
                        lower_type_ref_id(hir, *decl_ref, &analysis.registry, arena)
                {
                    let r = expr_byte_range(hir, *init_id);
                    check_assign(
                        analysis,
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
                validate_block(hir, analysis, arena, bool_t, then_branch, return_ty, diags);
                if let Some(eb) = else_branch {
                    validate_stmt(hir, analysis, arena, bool_t, *eb, return_ty, diags);
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
                validate_block(hir, analysis, arena, bool_t, body, return_ty, diags);
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
                validate_block(hir, analysis, arena, bool_t, body, return_ty, diags);
            }
            Stmt::For(ForStmt {
                condition, body, ..
            }) => {
                if let Some(c) = condition {
                    check_bool(analysis, arena, *c, bool_t, "for condition", hir, diags);
                }
                validate_block(hir, analysis, arena, bool_t, body, return_ty, diags);
            }
            Stmt::ForIn(ForInStmt { body, .. }) => {
                validate_block(hir, analysis, arena, bool_t, body, return_ty, diags);
            }
            Stmt::Try(TryStmt {
                try_block,
                catch_block,
                ..
            }) => {
                validate_block(hir, analysis, arena, bool_t, try_block, return_ty, diags);
                validate_block(hir, analysis, arena, bool_t, catch_block, return_ty, diags);
            }
            Stmt::At(AtStmt { block, .. }) => {
                validate_block(hir, analysis, arena, bool_t, block, return_ty, diags);
            }
            Stmt::Return(Some(v)) => {
                if let Some(rt) = return_ty {
                    let r = expr_byte_range(hir, *v);
                    check_assign(
                        analysis,
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
        if greycat_analyzer_types::is_assignable_to(arena, value_ty, declared_ty) {
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
        match &hir.exprs[expr_id] {
            greycat_analyzer_hir::types::Expr::Ident(name_idx) => {
                hir.idents[*name_idx].byte_range.clone()
            }
            other => other.byte_range(),
        }
    }
}

/// **P22** — project-wide TypeRef lowerer used by
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
    generics_in_scope: &HashMap<String, greycat_analyzer_types::GenericOwner>,
) -> greycat_analyzer_types::TypeId {
    use greycat_analyzer_types::Primitive;
    let tr = hir.type_refs[type_ref].clone();
    let name = hir.idents[tr.name].text.clone();
    let mut base = match name.as_str() {
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
                arena.generic(name.clone(), args)
            } else if let Some(owner) = generics_in_scope.get(&name) {
                arena.generic_param(name.clone(), owner.clone())
            } else if index.has_name(&name) {
                arena.named(name.clone())
            } else {
                arena.any()
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

/// P15.7 — arena-free intermediate for translating a foreign HIR
/// `TypeRef` into the destination module's `TypeArena`. Captures the
/// shape (primitive / named / generic / nullable) so phase 2 of
/// `infer_cross_module_call_types` can mint it without holding a
/// borrow on the foreign module.
#[derive(Debug, Clone)]
enum TypeShape {
    Primitive(greycat_analyzer_types::Primitive),
    Any,
    Null,
    Named {
        name: String,
        params: Vec<TypeShape>,
    },
    Optional(Box<TypeShape>),
}

/// Translate an existing `TypeId` in an arena back into a `TypeShape`.
/// Inverse of `mint_type_shape`. Used by 3.52 to harvest a foreign
/// iterable's generic args (already in the local arena thanks to 3.4)
/// without having to track the foreign HIR location.
fn read_type_id_shape(
    arena: &greycat_analyzer_types::TypeArena,
    type_id: greycat_analyzer_types::TypeId,
) -> TypeShape {
    let t = arena.get(type_id);
    let base = match &t.kind {
        greycat_analyzer_types::TypeKind::Primitive(p) => TypeShape::Primitive(*p),
        greycat_analyzer_types::TypeKind::Any => TypeShape::Any,
        greycat_analyzer_types::TypeKind::Null => TypeShape::Null,
        greycat_analyzer_types::TypeKind::Named { name } => TypeShape::Named {
            name: name.clone(),
            params: vec![],
        },
        greycat_analyzer_types::TypeKind::Generic { name, args } => TypeShape::Named {
            name: name.clone(),
            params: args.iter().map(|a| read_type_id_shape(arena, *a)).collect(),
        },
        // Falls through to `Any` for shapes that don't have a faithful
        // cross-arena `TypeShape` mapping yet (lambdas, unions, anonymous
        // structs). 3.52's caller drops `_` matches anyway, so this only
        // matters for *element* types of an iterable, where `any` is the
        // honest answer.
        _ => TypeShape::Any,
    };
    if t.nullable {
        TypeShape::Optional(Box::new(base))
    } else {
        base
    }
}

fn read_type_shape(
    hir: &Hir,
    type_ref_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeRef>,
) -> TypeShape {
    read_type_shape_subst(hir, type_ref_id, &HashMap::new())
}

/// `read_type_shape` extended with a generic-param substitution map.
/// When a `TypeRef`'s name matches a key in `subst`, the corresponding
/// `TypeShape` (already in the *caller's* arena namespace via
/// `read_type_id_shape`) replaces it. Powers cross-module generic
/// method-return / attr-type substitution: e.g. `nodeIndex<K, V>::get`
/// declared as `fn get(key: K): V?` resolves with `subst = {K → String,
/// V → node<Pkg>}` to `Optional(Named { name: "node", params: [Named "Pkg"] })`.
fn read_type_shape_subst(
    hir: &Hir,
    type_ref_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeRef>,
    subst: &HashMap<String, TypeShape>,
) -> TypeShape {
    use greycat_analyzer_types::Primitive;
    let tr = &hir.type_refs[type_ref_id];
    let name = hir.idents[tr.name].text.as_str();
    if let Some(replacement) = subst.get(name) {
        // Generic-param substitution wins, including over the optional
        // marker — the substituted TypeShape may itself already be
        // optional. Wrap nullable when the *use site's* TypeRef was
        // `T?` (only if not already optional).
        let r = replacement.clone();
        return if tr.optional && !matches!(&r, TypeShape::Optional(_)) {
            TypeShape::Optional(Box::new(r))
        } else {
            r
        };
    }
    let base = match name {
        "bool" => TypeShape::Primitive(Primitive::Bool),
        "int" => TypeShape::Primitive(Primitive::Int),
        "float" => TypeShape::Primitive(Primitive::Float),
        "char" => TypeShape::Primitive(Primitive::Char),
        "String" => TypeShape::Primitive(Primitive::String),
        "time" => TypeShape::Primitive(Primitive::Time),
        "duration" => TypeShape::Primitive(Primitive::Duration),
        "geo" => TypeShape::Primitive(Primitive::Geo),
        "any" => TypeShape::Any,
        "null" => TypeShape::Null,
        _ => {
            let params: Vec<TypeShape> = tr
                .params
                .iter()
                .map(|p| read_type_shape_subst(hir, *p, subst))
                .collect();
            TypeShape::Named {
                name: name.to_string(),
                params,
            }
        }
    };
    if tr.optional {
        TypeShape::Optional(Box::new(base))
    } else {
        base
    }
}

fn mint_type_shape(
    shape: &TypeShape,
    arena: &mut greycat_analyzer_types::TypeArena,
) -> greycat_analyzer_types::TypeId {
    match shape {
        TypeShape::Primitive(p) => arena.primitive(*p),
        TypeShape::Any => arena.any(),
        TypeShape::Null => arena.null(),
        TypeShape::Named { name, params } => {
            if params.is_empty() {
                arena.named(name.clone())
            } else {
                let args: Vec<_> = params.iter().map(|p| mint_type_shape(p, arena)).collect();
                arena.generic(name.clone(), args)
            }
        }
        TypeShape::Optional(inner) => {
            let id = mint_type_shape(inner, arena);
            arena.nullable(id)
        }
    }
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
