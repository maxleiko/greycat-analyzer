use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::{Hir, types::Decl};

use crate::resolver::Resolutions;

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Warn when a top-level `fn` / `type` / `enum` / `var` is never
/// referenced anywhere in the module *and* doesn't carry a runtime-
/// exposing annotation (`@expose`). The reference count
/// comes from `Resolutions::references_to`, which the resolver builds
/// from every `Definition::Decl` use site.
pub struct UnusedDecl;

impl LintRule for UnusedDecl {
    fn name(&self) -> &'static str {
        "unused-decl"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        check_unused_decl(cx.hir, cx.res, cx.symbols, &mut candidates);
        for d in candidates {
            cx.emit(d);
        }
    }
}

fn check_unused_decl(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    out: &mut Vec<LintDiagnostic>,
) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    for decl_id in &module.decls {
        let decl = &hir.decls[*decl_id];
        // Pragmas + native fns don't represent user
        // code that could be "unused" in a meaningful way.
        let (name_idx, modifiers, kind) = match decl {
            Decl::Fn(fnd) => (fnd.name, &fnd.modifiers, "fn"),
            Decl::Type(td) => (td.name, &td.modifiers, "type"),
            Decl::Enum(ed) => (ed.name, &ed.modifiers, "enum"),
            Decl::Var(vd) => (vd.name, &vd.modifiers, "var"),
            Decl::Pragma(_) => continue,
        };
        if modifiers.native {
            continue;
        }
        if !modifiers.private {
            continue;
        }
        if is_exposed(symbols, modifiers) {
            continue;
        }
        let ident = &hir.idents[name_idx];
        let name = &symbols[ident.symbol];
        if name.starts_with('_') {
            continue;
        }
        let count = res.references_to.get(decl_id).copied().unwrap_or(0);
        if count == 0 {
            out.push(LintDiagnostic {
                rule: "unused-decl",
                severity: LintSeverity::Warning,
                message: format!("unused private {kind} `{name}`"),
                byte_range: ident.byte_range.clone(),
                tag: None,
            });
        }
    }
}

fn is_exposed(
    symbols: &greycat_analyzer_core::SymbolTable,
    modifiers: &greycat_analyzer_hir::types::Modifiers,
) -> bool {
    modifiers
        .annotations
        .iter()
        .any(|a| &symbols[a.name.symbol] == "expose")
}
