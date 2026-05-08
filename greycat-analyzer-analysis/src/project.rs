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
