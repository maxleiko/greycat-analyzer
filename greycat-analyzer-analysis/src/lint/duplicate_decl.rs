use rustc_hash::FxHashMap;

use crate::index::Namespace;

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Error when two top-level decls share a name in the same module.
/// Mirrors the TS reference declarator's `Type 'X' is already
/// declared` / `Identifier 'X' is already declared` checks
/// (`packages/lang/src/analysis/declarator.ts:130`).
pub struct DuplicateDecl;

impl LintRule for DuplicateDecl {
    fn name(&self) -> &'static str {
        "duplicate-decl"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        // Bucket by `(symbol, namespace)` — GreyCat lets a type/enum
        // and a fn/var share an identifier (see `lib/std/core.gcl`'s
        // `type geo` plus `fn geo(...)`). Collisions only fire when
        // two decls land in the same namespace.
        let mut seen: FxHashMap<(greycat_analyzer_core::Symbol, Namespace), ()> =
            FxHashMap::default();
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        for decl_id in &module.decls {
            let decl = &cx.hir.decls[*decl_id];
            let Some(name_id) = decl.name() else {
                continue;
            };
            let Some(ns) = Namespace::of_decl(decl) else {
                continue;
            };
            let ident = &cx.hir.idents[name_id];
            if seen.insert((ident.symbol, ns), ()).is_some() {
                let name = &cx.symbols[ident.symbol];
                candidates.push(LintDiagnostic {
                    rule: "duplicate-decl",
                    severity: LintSeverity::Error,
                    message: format!("identifier `{name}` is already declared"),
                    byte_range: ident.byte_range.clone(),
                    tag: None,
                });
            }
        }
        for d in candidates {
            cx.emit(d);
        }
    }
}
