use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::{
    Hir,
    types::{Decl, FnDecl},
};

use crate::resolver::{Definition, Resolutions};

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// methods on a type (the param may be required for trait-shape
/// reasons) and skips parameters whose name starts with `_`.
pub struct UnusedParam;

impl LintRule for UnusedParam {
    fn name(&self) -> &'static str {
        "unused-param"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &cx.hir.decls[*decl_id] {
                Decl::Fn(fnd) => check_fn_params(
                    cx.hir,
                    cx.res,
                    cx.symbols,
                    fnd,
                    &mut candidates,
                    self.name(),
                ),
                Decl::Type(td) => {
                    for method_id in &td.methods {
                        if let Decl::Fn(fnd) = &cx.hir.decls[*method_id] {
                            check_fn_params(
                                cx.hir,
                                cx.res,
                                cx.symbols,
                                fnd,
                                &mut candidates,
                                self.name(),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        for d in candidates {
            cx.emit(d);
        }
    }
}

fn check_fn_params(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    if fnd.modifiers.native || fnd.body.is_none() {
        return;
    }
    for param_id in &fnd.params {
        let param = &hir.fn_params[*param_id];
        let ident = &hir.idents[param.name];
        let name = &symbols[ident.symbol];
        if name.starts_with('_') {
            continue;
        }
        let used = res.uses.values().any(|d| match d {
            Definition::Param(name) => *name == param.name,
            _ => false,
        });
        if !used {
            out.push(LintDiagnostic {
                rule,
                severity: LintSeverity::Warning,
                message: format!("unused parameter `{name}`"),
                byte_range: ident.byte_range.clone(),
                tag: None,
            });
        }
    }
}
