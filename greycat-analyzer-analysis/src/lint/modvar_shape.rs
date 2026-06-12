//! Module variables (`var` at top level) are GreyCat's persistent-store
//! roots and the runtime constrains their type tightly. The TS reference
//! emits three distinct errors (`Module variable type must be one of …`,
//! `Nodes are automatically initialized by GreyCat, they cannot be null`,
//! and the per-node-collection inner-shape rules). We mirror them here
//! as a pure-HIR rule (no typing context needed — the type *ref* spelling
//! is what's constrained).

use greycat_analyzer_hir::types::Decl;

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Three sibling sub-rules driven from one HIR walk.
///
/// - `modvar-must-be-node-tag` — top-level `var T` must use one of the
///   node-tag names: `node`, `nodeTime`, `nodeList`, `nodeIndex`, `nodeGeo`.
/// - `modvar-node-cannot-be-nullable` — the outer node-tag cannot carry
///   `?` (nodes are auto-initialized). Quickfix: drop the trailing `?`.
/// - `modvar-node-inner-must-be-nullable` — `node<T>` requires `T?`.
pub struct ModVarShape;

impl LintRule for ModVarShape {
    fn name(&self) -> &'static str {
        "modvar-shape"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        for decl_id in &module.decls {
            let Decl::Var(vd) = &cx.hir.decls[*decl_id] else {
                continue;
            };
            let Some(ty_ref) = vd.ty else {
                continue;
            };
            let ty = &cx.hir.type_refs[ty_ref];
            let head = &cx.symbols[cx.hir.idents[ty.name].symbol];
            // Syntactic-level lint: rejects everything except the
            // five node-tag head names. Pure source-level pattern,
            // no decl handle involved (the lint fires before
            // signature lowering populates type tables).
            let is_node_tag_head = matches!(
                head,
                "node" | "nodeTime" | "nodeGeo" | "nodeList" | "nodeIndex"
            );
            if !is_node_tag_head {
                candidates.push(LintDiagnostic {
                    rule: "modvar-must-be-node-tag",
                    severity: LintSeverity::Error,
                    message: "module variable type must be one of: \
                              `node<T?>`, `nodeTime<T>`, `nodeList<T>`, \
                              `nodeIndex<K, V>`, or `nodeGeo<T>`"
                        .into(),
                    byte_range: cx.hir.idents[vd.name].byte_range.clone(),
                    tag: None,
                });
                continue;
            }
            if ty.optional {
                candidates.push(LintDiagnostic {
                    rule: "modvar-node-cannot-be-nullable",
                    severity: LintSeverity::Error,
                    message: "nodes are automatically initialized by GreyCat \
                              and cannot be null — drop the trailing `?`"
                        .into(),
                    byte_range: ty.byte_range.clone(),
                    tag: None,
                });
            }
            if head == "node"
                && let Some(inner_ref) = ty.params.first()
            {
                let inner = &cx.hir.type_refs[*inner_ref];
                if !inner.optional {
                    candidates.push(LintDiagnostic {
                        rule: "modvar-node-inner-must-be-nullable",
                        severity: LintSeverity::Error,
                        message: "`node<T>` requires a nullable inner type — \
                                  use `node<T?>`"
                            .into(),
                        byte_range: inner.byte_range.clone(),
                        tag: None,
                    });
                }
            }
        }
        for d in candidates {
            cx.emit(d);
        }
    }
}
