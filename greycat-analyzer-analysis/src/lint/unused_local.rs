use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::{
    Hir,
    hir::{Decl, FnDecl, TypeDecl},
};

use crate::resolver::Resolutions;

use super::{LintCx, LintDiagnostic, LintRule};

/// Warn when a local `var name = …;` is bound but never read.
pub struct UnusedLocal;

impl LintRule for UnusedLocal {
    fn name(&self) -> &'static str {
        "unused-local"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &cx.hir.decls[*decl_id] {
                Decl::Fn(fnd) => check_fn(
                    cx.hir,
                    cx.res,
                    cx.symbols,
                    fnd,
                    &mut candidates,
                    self.name(),
                ),
                Decl::Type(td) => {
                    check_type(cx.hir, cx.res, cx.symbols, td, &mut candidates, self.name())
                }
                _ => {}
            }
        }
        for d in candidates {
            cx.emit(d);
        }
    }
}

fn check_fn(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    let Some(body) = fnd.body else {
        return;
    };
    super::visit_for_locals(hir, res, symbols, body, out, rule);
}

fn check_type(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    td: &TypeDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    for method_id in &td.methods {
        if let Decl::Fn(fnd) = &hir.decls[*method_id] {
            check_fn(hir, res, symbols, fnd, out, rule);
        }
    }
}
