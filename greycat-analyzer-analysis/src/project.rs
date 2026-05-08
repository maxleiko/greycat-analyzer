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
use greycat_analyzer_hir::types::{Decl, Ident};
use greycat_analyzer_hir::{Hir, lower_module};

use crate::analyzer::{AnalysisResult, ForeignMember, MemberDef, analyze_with_index};
use crate::lint::{LintDiagnostic, run_lints};
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
/// instead of lingering. The per-module [`AnalysisResult`] still owns
/// its own [`greycat_analyzer_types::TypeArena`] for now — wiring the
/// shared arena through the analyzer is **P6.2**.
#[derive(Debug, Default)]
pub struct ProjectAnalysis {
    pub index: ProjectIndex,
    modules: HashMap<Uri, ModuleAnalysis>,
}

impl ProjectAnalysis {
    pub fn new() -> Self {
        Self {
            index: ProjectIndex::new(),
            modules: HashMap::new(),
        }
    }

    /// One-pass build over every document currently in `manager`.
    pub fn analyze(manager: &SourceManager) -> Self {
        let mut out = Self::new();
        out.rebuild(manager);
        out
    }

    /// Rebuild from scratch over the current `manager` state. Existing
    /// cache entries are dropped.
    pub fn rebuild(&mut self, manager: &SourceManager) {
        self.index = ProjectIndex::new();
        self.modules.clear();

        // Pass 1: lower every doc to HIR and ingest into the project
        // index so types declared in one module are visible to peers.
        let mut hirs: Vec<(Uri, Hir, Duration)> = Vec::with_capacity(manager.len());
        for (uri, cell) in manager.iter() {
            let doc = cell.borrow();
            let lower_start = Instant::now();
            let hir = lower_module(&doc.text, "module", &doc.lib, doc.root_node());
            let lower_took = lower_start.elapsed();
            self.index.ingest(uri, &hir);
            hirs.push((uri.clone(), hir, lower_took));
        }

        // Pass 2: per-module resolver + analyzer + lints. The per-module
        // analyzer still owns its own arena; P6.2 reroutes the lookups.
        for (uri, hir, lower_took) in hirs {
            let mut timings = ModuleTimings {
                lower: lower_took,
                ..ModuleTimings::default()
            };
            let t0 = Instant::now();
            let resolutions = resolve_with_index(&hir, &self.index);
            timings.resolve = t0.elapsed();
            let t1 = Instant::now();
            let analysis = analyze_with_index(&hir, &resolutions, &self.index);
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
                    timings,
                },
            );
        }

        // Pass 3 (P11.5): cross-module member resolution. Drain each
        // module's `deferred_member_uses` — `(property_ident, type_name)`
        // pairs the analyzer couldn't bind because the receiver's type
        // wasn't declared in that module — and resolve them through the
        // global decl table.
        self.resolve_cross_module_members();

        // Pass 3.5 (P15.7): cross-module call return-type inference.
        // Walks every module's `Expr::Call` whose callee is `Expr::Static`
        // bound to a foreign method, looks up the foreign method's
        // declared return type, translates it into the current module's
        // type arena, and updates `analysis.expr_types[call_id]`.
        self.infer_cross_module_call_types();

        // Pass 4 (P14.9): bump `references_to` for every decl that's
        // referenced from another module via a qualified-name access
        // (`<module>::<name>`, `<module>::<type>::<name>`, etc.). Lets
        // the unused-decl lint correctly skip `private` decls that
        // are referenced through their fully-qualified name from
        // elsewhere in the project.
        self.compute_qualified_refs(manager);
    }

    /// Walk each module's `deferred_member_uses` and bind the foreign
    /// attr / method via [`ProjectIndex::locate_decl`]. Idempotent —
    /// re-running drains an already-empty list. (P11.5.)
    fn resolve_cross_module_members(&mut self) {
        #[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
        let mut updates: HashMap<Uri, Vec<(Idx<Ident>, ForeignMember)>> = HashMap::new();
        for (cur_uri, cur_module) in &self.modules {
            for (property_idx, type_name) in &cur_module.analysis.deferred_member_uses {
                let prop_text = cur_module.hir.idents[*property_idx].text.clone();
                let Some((foreign_uri, foreign_decl_id)) =
                    self.index.locate_decl(type_name).first()
                else {
                    continue;
                };
                let Some(foreign_module) = self.modules.get(foreign_uri) else {
                    continue;
                };
                let Decl::Type(ftd) = &foreign_module.hir.decls[*foreign_decl_id] else {
                    continue;
                };
                let mut bound = false;
                for attr_id in &ftd.attrs {
                    let attr_name = &foreign_module.hir.idents
                        [foreign_module.hir.type_attrs[*attr_id].name]
                        .text;
                    if *attr_name == prop_text {
                        updates.entry(cur_uri.clone()).or_default().push((
                            *property_idx,
                            ForeignMember {
                                uri: foreign_uri.clone(),
                                member: MemberDef::Attr(*attr_id),
                            },
                        ));
                        bound = true;
                        break;
                    }
                }
                if bound {
                    continue;
                }
                for method_id in &ftd.methods {
                    let Decl::Fn(m) = &foreign_module.hir.decls[*method_id] else {
                        continue;
                    };
                    if foreign_module.hir.idents[m.name].text == prop_text {
                        updates.entry(cur_uri.clone()).or_default().push((
                            *property_idx,
                            ForeignMember {
                                uri: foreign_uri.clone(),
                                member: MemberDef::Method(*method_id),
                            },
                        ));
                        break;
                    }
                }
            }
        }
        for (uri, entries) in updates {
            if let Some(m) = self.modules.get_mut(&uri) {
                for (prop_idx, fm) in entries {
                    m.analysis.foreign_member_uses.insert(prop_idx, fm);
                }
            }
        }
    }

    /// P15.7 — cross-module call return-type inference. After
    /// [`Self::resolve_cross_module_members`] populates
    /// `foreign_member_uses`, walk every module's HIR `Expr::Call`s
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
    fn infer_cross_module_call_types(&mut self) {
        use greycat_analyzer_hir::types::{Expr, Stmt};

        // Phase 1 — read-only: collect the type-shape that each
        // affected expr should carry, plus a list of Stmt::Var whose
        // init expr feeds into one of those updates so we can re-link
        // their `def_types` afterwards.
        #[allow(clippy::mutable_key_type)]
        let mut expr_updates: HashMap<Uri, Vec<(Idx<Expr>, TypeShape)>> = HashMap::new();
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
                let shape = match resolve_static_member_shape(&self.modules, cur_module, s) {
                    Some(sh) => sh,
                    None => continue,
                };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((static_id, shape));
            }
            // 1b) Call(Static) — overrides the static expr_type and
            // also drives the call's return-type inference.
            for (call_id, call_expr) in cur_module.hir.exprs.iter() {
                let Expr::Call(call) = call_expr else {
                    continue;
                };
                let Expr::Static(s) = &cur_module.hir.exprs[call.callee] else {
                    continue;
                };
                let Some(shape) = resolve_static_call_return_shape(&self.modules, cur_module, s)
                else {
                    continue;
                };
                expr_updates
                    .entry(cur_uri.clone())
                    .or_default()
                    .push((call_id, shape));
            }
        }

        // Phase 2 — mutable: mint the snapshotted shapes into each
        // module's TypeArena and update `expr_types`. Then walk
        // `Stmt::Var` to re-link `def_types` for locals whose init
        // expr we just updated.
        for (uri, entries) in expr_updates {
            let Some(m) = self.modules.get_mut(&uri) else {
                continue;
            };
            // Build a small index of which exprs we touched.
            let mut touched: HashMap<Idx<Expr>, greycat_analyzer_types::TypeId> = HashMap::new();
            for (expr_id, shape) in entries {
                let ty = mint_type_shape(&shape, &mut m.analysis.types);
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
        let changed_hir = manager.get(uri).map(|cell| {
            let doc = cell.borrow();
            let start = Instant::now();
            let hir = lower_module(&doc.text, "module", &doc.lib, doc.root_node());
            lower_took = start.elapsed();
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
        let analysis = analyze_with_index(&hir, &resolutions, &self.index);
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
                timings,
            },
        );
        // P11.5: re-resolve cross-module member bindings whenever a doc
        // is invalidated. Cheap because `deferred_member_uses` is small
        // per module and the work is purely table-lookup.
        self.resolve_cross_module_members();
        // P14.9: re-derive qualified-name reference counts.
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

/// P15.7 — figure out what type a standalone `Expr::Static`
/// (`Type::name` *not* immediately followed by a call) should carry.
/// Method references are `function`; attr references are `field`. This
/// matches GreyCat's reflection model where `Type::method` yields a
/// callable function value and `Type::attr` yields a field handle.
#[allow(clippy::mutable_key_type)] // lsp_types::Uri is fine as a key in practice.
fn resolve_static_member_shape(
    modules: &HashMap<Uri, ModuleAnalysis>,
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

fn read_type_shape(
    hir: &Hir,
    type_ref_id: greycat_analyzer_hir::arena::Idx<greycat_analyzer_hir::types::TypeRef>,
) -> TypeShape {
    use greycat_analyzer_types::Primitive;
    let tr = &hir.type_refs[type_ref_id];
    let name = hir.idents[tr.name].text.as_str();
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
            let params: Vec<TypeShape> =
                tr.params.iter().map(|p| read_type_shape(hir, *p)).collect();
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
}
