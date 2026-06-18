use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::{
    Hir,
    arena::Idx,
    hir::{Decl, FnDecl, Ident, TypeDecl},
};

use crate::resolver::{Definition, Resolutions};

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Warn when a `type Foo<T>` / `fn foo<T>(...)` declares a generic
/// parameter that is never referenced inside its enclosing decl
/// (attributes, supertype, methods, params, return type, body).
/// Skips:
/// - native / abstract decls: the generic may participate in a runtime
///   shape contract that the body can't see (e.g. `native type Array<T>`).
/// - names starting with `_`, matching the convention `unused-param`
///   uses for intentional unused params.
pub struct UnusedGenericParam;

impl LintRule for UnusedGenericParam {
    fn name(&self) -> &'static str {
        "unused-generic-param"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &cx.hir.decls[*decl_id] {
                Decl::Fn(fnd) => {
                    check_fn_generics(
                        cx.hir,
                        cx.res,
                        cx.symbols,
                        fnd,
                        &mut candidates,
                        self.name(),
                    );
                }
                Decl::Type(td) => {
                    check_type_generics(
                        cx.hir,
                        cx.res,
                        cx.symbols,
                        td,
                        &mut candidates,
                        self.name(),
                    );
                    // Methods are themselves `Decl::Fn` — their own
                    // generics (separate from the enclosing type's)
                    // get checked here too.
                    for method_id in &td.methods {
                        if let Decl::Fn(method) = &cx.hir.decls[*method_id] {
                            check_fn_generics(
                                cx.hir,
                                cx.res,
                                cx.symbols,
                                method,
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

fn check_fn_generics(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    if fnd.modifiers.native {
        return;
    }
    emit_unused_generics(hir, res, symbols, &fnd.generics, out, rule);
}

fn check_type_generics(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    td: &TypeDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    if td.modifiers.native {
        return;
    }
    emit_unused_generics(hir, res, symbols, &td.generics, out, rule);
}

fn emit_unused_generics(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    generics: &[Idx<Ident>],
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    for g in generics {
        let ident: &Ident = &hir.idents[*g];
        let name = &symbols[ident.symbol];
        if name.starts_with('_') {
            continue;
        }
        let used = res.uses.values().any(|d| match d {
            Definition::Generic(name) => *name == *g,
            _ => false,
        });
        if !used {
            out.push(LintDiagnostic {
                rule,
                severity: LintSeverity::Warning,
                message: format!("unused generic parameter `{name}`"),
                byte_range: ident.byte_range.clone(),
                tag: None,
            });
        }
    }
}
