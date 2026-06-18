use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::{
    Hir,
    arena::Idx,
    hir::{Decl, Stmt},
};

use crate::resolver::{Definition, Resolutions};

use super::{LintCx, LintDiagnostic, LintRule, LintSeverity};

/// Warn when a `catch (e)` parameter is bound but never read inside
/// the catch block. Distinct from [`UnusedParam`] because the auto-fix
/// is qualitatively different — a fn param can't disappear (it's part
/// of the signature), but a catch ident has the bare `catch { … }`
/// form to fall back to. The fix drops `(e)` entirely instead of
/// renaming to `_e`.
pub struct UnusedCatchParam;

impl LintRule for UnusedCatchParam {
    fn name(&self) -> &'static str {
        "unused-catch-param"
    }

    fn check(&self, cx: &mut LintCx<'_>) {
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &cx.hir.decls[*decl_id] {
                Decl::Fn(fnd) => {
                    if let Some(body) = fnd.body {
                        visit_for_catch_params(
                            cx.hir,
                            cx.res,
                            cx.symbols,
                            body,
                            &mut candidates,
                            self.name(),
                        );
                    }
                }
                Decl::Type(td) => {
                    for method_id in &td.methods {
                        if let Decl::Fn(fnd) = &cx.hir.decls[*method_id]
                            && let Some(body) = fnd.body
                        {
                            visit_for_catch_params(
                                cx.hir,
                                cx.res,
                                cx.symbols,
                                body,
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

/// Walk a fn body looking for `try { … } catch (e) { … }` shapes whose
/// `e` is never read. The catch param is bound by the resolver as
/// `Definition::Local(name)` (same as a `var`), so the usage check
/// mirrors `unused-local`'s — emit under the caller-supplied rule
/// name (today: `unused-catch-param`).
fn visit_for_catch_params(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    stmt_id: Idx<Stmt>,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    use greycat_analyzer_hir::hir::BlockStmt;
    fn visit_block(
        hir: &Hir,
        res: &Resolutions,
        symbols: &SymbolTable,
        block: &BlockStmt,
        out: &mut Vec<LintDiagnostic>,
        rule: &'static str,
    ) {
        for s in &block.stmts {
            visit_for_catch_params(hir, res, symbols, *s, out, rule);
        }
    }
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => visit_block(hir, res, symbols, b, out, rule),
        Stmt::If(i) => {
            visit_block(hir, res, symbols, &i.then_branch, out, rule);
            if let Some(eb) = i.else_branch {
                visit_for_catch_params(hir, res, symbols, eb, out, rule);
            }
        }
        Stmt::While(w) => visit_block(hir, res, symbols, &w.body, out, rule),
        Stmt::DoWhile(w) => visit_block(hir, res, symbols, &w.body, out, rule),
        Stmt::For(f) => visit_block(hir, res, symbols, &f.body, out, rule),
        Stmt::ForIn(f) => visit_block(hir, res, symbols, &f.body, out, rule),
        Stmt::Try(t) => {
            visit_block(hir, res, symbols, &t.try_block, out, rule);
            if let Some(name) = t.error_param {
                let ident = &hir.idents[name];
                let ident_name = &symbols[ident.symbol];
                if !ident_name.starts_with('_') {
                    let used = res.uses.values().any(|d| match d {
                        Definition::Local(n) => *n == name,
                        _ => false,
                    });
                    if !used {
                        out.push(LintDiagnostic {
                            rule,
                            severity: LintSeverity::Warning,
                            message: format!("unused catch parameter `{ident_name}`"),
                            byte_range: ident.byte_range.clone(),
                            tag: None,
                        });
                    }
                }
            }
            visit_block(hir, res, symbols, &t.catch_block, out, rule);
        }
        Stmt::At(a) => visit_block(hir, res, symbols, &a.block, out, rule),
        _ => {}
    }
}
