//! Linter rules (P4.2).
//!
//! A small rule-based framework on top of HIR + Resolutions. Each rule
//! is a trait impl that walks the module and emits [`LintDiagnostic`]s.
//! Rules are stable, named (so they can be configured / suppressed in
//! future), and pure — they don't mutate the inputs.
//!
//! Ports the *rule* slice of `packages/cli/src/lint/` (~242 LoC of TS
//! plus rules embedded in analyzer.ts). The fix-application driver
//! (sort edits, apply non-overlapping ones, retry) is deferred until
//! the LSP code-action layer has concrete edit suggestions to apply
//! (P3.6 placeholder).

use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, FnDecl, Ident, Stmt, TypeDecl};

use crate::resolver::{Definition, Resolutions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Warning,
    Hint,
}

#[derive(Debug, Clone)]
pub struct LintDiagnostic {
    pub rule: &'static str,
    pub severity: LintSeverity,
    pub message: String,
    pub byte_range: Range<usize>,
}

/// Trait every lint rule implements. Impls walk `hir` / `res` and push
/// findings into the supplied vector.
pub trait LintRule {
    fn name(&self) -> &'static str;
    fn check(&self, hir: &Hir, res: &Resolutions, out: &mut Vec<LintDiagnostic>);
}

/// Run every registered rule in order and return the merged findings.
pub fn run_lints(hir: &Hir, res: &Resolutions) -> Vec<LintDiagnostic> {
    let rules: Vec<Box<dyn LintRule>> = vec![Box::new(UnusedLocal), Box::new(UnusedParam)];
    let mut out = Vec::new();
    for rule in rules {
        rule.check(hir, res, &mut out);
    }
    out
}

// =============================================================================
// Rule: unused-local
// =============================================================================

/// Warn when a local `var name = …;` is bound but never read.
pub struct UnusedLocal;

impl LintRule for UnusedLocal {
    fn name(&self) -> &'static str {
        "unused-local"
    }

    fn check(&self, hir: &Hir, res: &Resolutions, out: &mut Vec<LintDiagnostic>) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &hir.decls[*decl_id] {
                Decl::Fn(fnd) => check_fn(hir, res, fnd, out, self.name()),
                Decl::Type(td) => check_type(hir, res, td, out, self.name()),
                _ => {}
            }
        }
    }
}

fn check_fn(hir: &Hir, res: &Resolutions, fnd: &FnDecl, out: &mut Vec<LintDiagnostic>, rule: &'static str) {
    let Some(body) = fnd.body else {
        return;
    };
    visit_for_locals(hir, res, body, out, rule);
}

fn check_type(hir: &Hir, res: &Resolutions, td: &TypeDecl, out: &mut Vec<LintDiagnostic>, rule: &'static str) {
    for method_id in &td.methods {
        if let Decl::Fn(fnd) = &hir.decls[*method_id] {
            check_fn(hir, res, fnd, out, rule);
        }
    }
}

fn visit_for_locals(
    hir: &Hir,
    res: &Resolutions,
    stmt_id: Idx<Stmt>,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(stmts) => {
            for s in stmts {
                visit_for_locals(hir, res, *s, out, rule);
            }
        }
        Stmt::Var(v) => {
            // Was `v.name` referenced anywhere as a Local?
            let used = res.uses.values().any(|d| match d {
                Definition::Local(name) => *name == v.name,
                _ => false,
            });
            if !used {
                let ident = &hir.idents[v.name];
                out.push(LintDiagnostic {
                    rule,
                    severity: LintSeverity::Warning,
                    message: format!("unused local `{}`", ident.text),
                    byte_range: ident.byte_range.clone(),
                });
            }
        }
        Stmt::If(i) => {
            visit_for_locals(hir, res, i.then_branch, out, rule);
            if let Some(eb) = i.else_branch {
                visit_for_locals(hir, res, eb, out, rule);
            }
        }
        Stmt::While(w) => visit_for_locals(hir, res, w.body, out, rule),
        Stmt::DoWhile(w) => visit_for_locals(hir, res, w.body, out, rule),
        Stmt::For(f) => visit_for_locals(hir, res, f.body, out, rule),
        Stmt::ForIn(f) => visit_for_locals(hir, res, f.body, out, rule),
        Stmt::Try(t) => {
            visit_for_locals(hir, res, t.try_block, out, rule);
            visit_for_locals(hir, res, t.catch_block, out, rule);
        }
        Stmt::At(a) => visit_for_locals(hir, res, a.block, out, rule),
        _ => {}
    }
}

// =============================================================================
// Rule: unused-param
// =============================================================================

/// Hint when a function parameter is never read in its body. Skips
/// methods on a type (the param may be required for trait-shape
/// reasons) and skips parameters whose name starts with `_`.
pub struct UnusedParam;

impl LintRule for UnusedParam {
    fn name(&self) -> &'static str {
        "unused-param"
    }

    fn check(&self, hir: &Hir, res: &Resolutions, out: &mut Vec<LintDiagnostic>) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            if let Decl::Fn(fnd) = &hir.decls[*decl_id] {
                check_fn_params(hir, res, fnd, out, self.name());
            }
        }
    }
}

fn check_fn_params(
    hir: &Hir,
    res: &Resolutions,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    if fnd.modifiers.native || fnd.modifiers.abstract_ {
        return;
    }
    if fnd.body.is_none() {
        return;
    }
    for param_id in &fnd.params {
        let param = &hir.fn_params[*param_id];
        let ident: &Ident = &hir.idents[param.name];
        if ident.text.starts_with('_') {
            continue;
        }
        let used = res.uses.values().any(|d| match d {
            Definition::Param(name) => *name == param.name,
            _ => false,
        });
        if !used {
            out.push(LintDiagnostic {
                rule,
                severity: LintSeverity::Hint,
                message: format!("unused parameter `{}`", ident.text),
                byte_range: ident.byte_range.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::resolve;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;

    fn lint(src: &str) -> Vec<LintDiagnostic> {
        let tree = parse(src);
        let hir = lower_module(src, "mod", "project", tree.root_node());
        let res = resolve(&hir);
        run_lints(&hir, &res)
    }

    #[test]
    fn unused_local_is_warned() {
        let diags = lint(
            r#"
fn f(): int {
    var x: int = 0;
    return 42;
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "unused-local" && d.message.contains("`x`")),
            "expected unused-local on x: {diags:?}"
        );
    }

    #[test]
    fn used_local_is_silent() {
        let diags = lint(
            r#"
fn f(): int {
    var x: int = 1;
    return x;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unused-local"),
            "expected no unused-local, got {diags:?}"
        );
    }

    #[test]
    fn unused_param_is_hinted() {
        let diags = lint(
            r#"
fn f(x: int, y: int): int {
    return x;
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "unused-param" && d.message.contains("`y`")),
            "expected unused-param on y: {diags:?}"
        );
    }

    #[test]
    fn underscore_param_skipped() {
        let diags = lint(
            r#"
fn f(_unused: int): int {
    return 0;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unused-param"),
            "underscore-prefixed params should not warn: {diags:?}"
        );
    }

    #[test]
    fn native_fn_params_skipped() {
        let diags = lint("private native fn read(path: String): String;\n");
        assert!(
            !diags.iter().any(|d| d.rule == "unused-param"),
            "native fns shouldn't trigger unused-param: {diags:?}"
        );
    }
}
