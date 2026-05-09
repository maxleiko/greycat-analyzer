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

use std::collections::HashMap;
use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, Expr, FnDecl, Ident, MemberExpr, Stmt, TypeDecl};
use greycat_analyzer_types::{TypeKind, is_node_tag};

use crate::analyzer::AnalysisResult;
use crate::resolver::{Definition, Resolutions};
use crate::stdlib::ProjectIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Error,
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
    let rules: Vec<Box<dyn LintRule>> = vec![
        Box::new(UnusedLocal),
        Box::new(UnusedParam),
        Box::new(UnusedDecl),
        Box::new(DuplicateDecl),
    ];
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

fn check_fn(
    hir: &Hir,
    res: &Resolutions,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    let Some(body) = fnd.body else {
        return;
    };
    visit_for_locals(hir, res, body, out, rule);
}

fn check_type(
    hir: &Hir,
    res: &Resolutions,
    td: &TypeDecl,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    for method_id in &td.methods {
        if let Decl::Fn(fnd) = &hir.decls[*method_id] {
            check_fn(hir, res, fnd, out, rule);
        }
    }
}

fn visit_block_for_locals(
    hir: &Hir,
    res: &Resolutions,
    block: &greycat_analyzer_hir::types::BlockStmt,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    for s in &block.stmts {
        visit_for_locals(hir, res, *s, out, rule);
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
        Stmt::Block(b) => visit_block_for_locals(hir, res, b, out, rule),
        Stmt::Var(v) => {
            // Convention (matches `unused-param` and Rust): a leading
            // `_` opts out of the unused warning. Lets users keep
            // `var _x = expr;` for typing / side-effect reasons
            // without the linter complaining.
            let ident = &hir.idents[v.name];
            if ident.text.starts_with('_') {
                return;
            }
            // Was `v.name` referenced anywhere as a Local?
            let used = res.uses.values().any(|d| match d {
                Definition::Local(name) => *name == v.name,
                _ => false,
            });
            if !used {
                out.push(LintDiagnostic {
                    rule,
                    severity: LintSeverity::Warning,
                    message: format!("unused local `{}`", ident.text),
                    byte_range: ident.byte_range.clone(),
                });
            }
        }
        Stmt::If(i) => {
            visit_block_for_locals(hir, res, &i.then_branch, out, rule);
            if let Some(eb) = i.else_branch {
                visit_for_locals(hir, res, eb, out, rule);
            }
        }
        Stmt::While(w) => visit_block_for_locals(hir, res, &w.body, out, rule),
        Stmt::DoWhile(w) => visit_block_for_locals(hir, res, &w.body, out, rule),
        Stmt::For(f) => visit_block_for_locals(hir, res, &f.body, out, rule),
        Stmt::ForIn(f) => visit_block_for_locals(hir, res, &f.body, out, rule),
        Stmt::Try(t) => {
            visit_block_for_locals(hir, res, &t.try_block, out, rule);
            visit_block_for_locals(hir, res, &t.catch_block, out, rule);
        }
        Stmt::At(a) => visit_block_for_locals(hir, res, &a.block, out, rule),
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
                severity: LintSeverity::Warning,
                message: format!("unused parameter `{}`", ident.text),
                byte_range: ident.byte_range.clone(),
            });
        }
    }
}

// =============================================================================
// Rule: unused-decl
// =============================================================================

/// Warn when a top-level `fn` / `type` / `enum` / `var` is never
/// referenced anywhere in the module *and* doesn't carry a runtime-
/// exposing annotation (`@expose`). Drives P6.7. The reference count
/// comes from `Resolutions::references_to`, which the resolver builds
/// from every `Definition::Decl` use site.
pub struct UnusedDecl;

impl LintRule for UnusedDecl {
    fn name(&self) -> &'static str {
        "unused-decl"
    }

    fn check(&self, hir: &Hir, res: &Resolutions, out: &mut Vec<LintDiagnostic>) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        let _ = module; // module-name / lib intentionally unused — the
        // gate is the `private` modifier (see below).
        for decl_id in &module.decls {
            let decl = &hir.decls[*decl_id];
            // Pragmas + native / abstract fns don't represent user
            // code that could be "unused" in a meaningful way.
            let (name_idx, modifiers, kind) = match decl {
                Decl::Fn(fnd) => (fnd.name, &fnd.modifiers, "fn"),
                Decl::Type(td) => (td.name, &td.modifiers, "type"),
                Decl::Enum(ed) => (ed.name, &ed.modifiers, "enum"),
                Decl::Var(vd) => (vd.name, &vd.modifiers, "var"),
                Decl::Pragma(_) => continue,
            };
            if modifiers.native || modifiers.abstract_ {
                continue;
            }
            // Only `private` decls are checked — anything non-private
            // is potentially called from outside this module (other
            // modules, stdlib consumers, runtime tooling) and we can't
            // see those use sites here.
            if !modifiers.private {
                continue;
            }
            // `@expose` (and other runtime-exposing annotations) keep
            // the decl alive even without intra-module references.
            if exposes_runtime(modifiers) {
                continue;
            }
            let ident = &hir.idents[name_idx];
            // Underscore-prefixed names are an opt-out marker, mirroring
            // the param convention.
            if ident.text.starts_with('_') {
                continue;
            }
            let count = res.references_to.get(decl_id).copied().unwrap_or(0);
            if count == 0 {
                out.push(LintDiagnostic {
                    rule: self.name(),
                    severity: LintSeverity::Warning,
                    message: format!("unused private {kind} `{}`", ident.text),
                    byte_range: ident.byte_range.clone(),
                });
            }
        }
    }
}

// =============================================================================
// Rule: duplicate-decl  (P13.6 — declarator.ts residual)
// =============================================================================

/// Error when two top-level decls share a name in the same module.
/// Mirrors the TS reference declarator's `Type 'X' is already
/// declared` / `Identifier 'X' is already declared` checks
/// (`packages/lang/src/analysis/declarator.ts:130`).
pub struct DuplicateDecl;

impl LintRule for DuplicateDecl {
    fn name(&self) -> &'static str {
        "duplicate-decl"
    }

    fn check(&self, hir: &Hir, _res: &Resolutions, out: &mut Vec<LintDiagnostic>) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        let mut seen: HashMap<String, ()> = HashMap::new();
        for decl_id in &module.decls {
            let Some(name_id) = hir.decls[*decl_id].name() else {
                continue;
            };
            // Skip pragma-pragma duplicates (multiple `@library` /
            // `@include` pragmas with the same key are normal).
            if matches!(&hir.decls[*decl_id], Decl::Pragma(_)) {
                continue;
            }
            let ident = &hir.idents[name_id];
            if seen.insert(ident.text.clone(), ()).is_some() {
                out.push(LintDiagnostic {
                    rule: self.name(),
                    severity: LintSeverity::Error,
                    message: format!("identifier `{}` is already declared", ident.text),
                    byte_range: ident.byte_range.clone(),
                });
            }
        }
    }
}

fn exposes_runtime(modifiers: &greycat_analyzer_hir::types::Modifiers) -> bool {
    modifiers.annotations.iter().any(|a| {
        matches!(
            a.name.as_str(),
            "expose" | "permission" | "role" | "library"
        )
    })
}

// =============================================================================
// Rule: arrow-on-non-deref (P16.6 — typed lint)
// =============================================================================

/// Walk every `Expr::Arrow` and emit an error when the receiver's type is
/// neither a node tag (`is_node_tag`) nor declared with `@deref(...)` in
/// the `ProjectIndex::type_flags` table. Mirrors the GreyCat runtime's
/// "cannot deref" rejection — caught at edit time rather than at run.
///
/// This is a *typed* lint: it depends on the per-module
/// [`AnalysisResult`] (for `expr_types`) and the project-wide
/// [`ProjectIndex`] (for `@deref` type flags), so it doesn't run as part
/// of [`run_lints`]. The project pipeline drives it after the
/// cross-module type fixups have settled — see
/// [`crate::project::ProjectAnalysis`].
///
/// Skipped (conservative) cases:
/// - `any` / `null` / `never` — no concrete type to check.
/// - `union` / `lambda` / `tuple` / `anonymous` / `enum` / `generic_param`
///   — no head name to look up. Better to under-warn than to fire on
///   shapes the lint hasn't been formally taught.
pub fn lint_arrow_on_non_deref(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &greycat_analyzer_types::TypeArena,
    index: &ProjectIndex,
    out: &mut Vec<LintDiagnostic>,
) {
    for (expr_id, expr) in hir.exprs.iter() {
        let Expr::Arrow(MemberExpr {
            receiver,
            byte_range,
            ..
        }) = expr
        else {
            continue;
        };
        let Some(recv_ty) = analysis.expr_types.get(receiver).copied() else {
            continue;
        };
        let head = receiver_head_name(arena, recv_ty);
        let Some(name) = head else {
            // Conservative: receiver is `any` / lambda / tuple / etc. —
            // no head name to classify. Skip.
            continue;
        };
        if is_node_tag(&name) {
            continue;
        }
        if index
            .type_flags_for(&name)
            .is_some_and(|f| f.deref.is_some())
        {
            continue;
        }
        let _ = expr_id;
        let display = greycat_analyzer_types::display(arena, recv_ty);
        out.push(LintDiagnostic {
            rule: "arrow-on-non-deref",
            severity: LintSeverity::Error,
            message: format!("`->` requires a node-tag or `@deref` receiver, got `{display}`"),
            byte_range: byte_range.clone(),
        });
    }
}

/// Extract the head name of `recv_ty` for `arrow-on-non-deref` dispatch.
/// Strips top-level nullability and reduces `Named` / `Generic` /
/// `Primitive` to their canonical name. Returns `None` for shapes the
/// lint conservatively skips (any / never / null / lambda / tuple /
/// anonymous / union / enum / generic-param).
fn receiver_head_name(
    arena: &greycat_analyzer_types::TypeArena,
    ty: greycat_analyzer_types::TypeId,
) -> Option<String> {
    let t = arena.get(ty);
    match &t.kind {
        TypeKind::Named { name } => Some(name.clone()),
        TypeKind::Generic { name, .. } => Some(name.clone()),
        TypeKind::Primitive(p) => Some(p.name().to_string()),
        _ => None,
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
            diags
                .iter()
                .any(|d| d.rule == "unused-local" && d.message.contains("`x`")),
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
            diags
                .iter()
                .any(|d| d.rule == "unused-param" && d.message.contains("`y`")),
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

    /// **P19.10 follow-up** — `var _name = expr;` opts out of the
    /// unused-local warning, matching `unused-param`'s behavior and
    /// the Rust convention. Lets users keep a binding for typing /
    /// side-effect reasons without the linter complaining.
    #[test]
    fn underscore_local_skipped() {
        let diags = lint(
            r#"
fn f(): int {
    var _ignored: int = 0;
    return 42;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unused-local"),
            "underscore-prefixed locals should not warn: {diags:?}"
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

    #[test]
    fn unused_private_fn_warns() {
        let diags = lint("private fn unused() {}\nprivate fn used() { used(); }\n");
        let unused: Vec<_> = diags.iter().filter(|d| d.rule == "unused-decl").collect();
        assert!(
            unused
                .iter()
                .any(|d| d.message.contains("unused private fn `unused`")),
            "expected unused-decl on private `unused`, got: {diags:?}"
        );
        // `used` references itself recursively → ref count 1 → not warned.
        assert!(
            !unused.iter().any(|d| d.message.contains("`used`")),
            "self-reference should suppress unused-decl: {diags:?}"
        );
    }

    #[test]
    fn non_private_decl_skipped() {
        let diags = lint("fn callable() {}\n");
        assert!(
            !diags.iter().any(|d| d.rule == "unused-decl"),
            "non-private top-level should not warn (callable from elsewhere): {diags:?}"
        );
    }

    #[test]
    fn exposed_decl_skipped() {
        let diags = lint("@expose\nprivate fn handler() {}\n");
        assert!(
            !diags.iter().any(|d| d.rule == "unused-decl"),
            "@expose should keep decl alive: {diags:?}"
        );
    }

    #[test]
    fn underscore_decl_skipped() {
        let diags = lint("private fn _scratch() {}\n");
        assert!(
            !diags.iter().any(|d| d.rule == "unused-decl"),
            "underscore-prefixed private should not warn: {diags:?}"
        );
    }

    #[test]
    fn duplicate_decl_flagged() {
        // P13.6: two top-level decls sharing a name surfaces a
        // `duplicate-decl` error.
        let diags = lint("fn foo() {}\nfn foo() {}\n");
        let dup: Vec<_> = diags
            .iter()
            .filter(|d| d.rule == "duplicate-decl")
            .collect();
        assert_eq!(dup.len(), 1, "expected one duplicate-decl: {diags:?}");
        assert!(dup[0].message.contains("foo"));
        assert_eq!(dup[0].severity, LintSeverity::Error);
    }

    #[test]
    fn duplicate_decl_distinct_names_silent() {
        let diags = lint("fn foo() {}\nfn bar() {}\n");
        assert!(
            !diags.iter().any(|d| d.rule == "duplicate-decl"),
            "distinct names should not flag: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // arrow-on-non-deref (P16.6) — exercised via the project pipeline so
    // the typed-lint pass actually fires (it consumes the analyzer's
    // `expr_types` table and the project-wide `ProjectIndex`).
    // -------------------------------------------------------------------

    fn project_lints(src: &str) -> Vec<LintDiagnostic> {
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        let pa = ProjectAnalysis::analyze(&mgr);
        pa.module(&uri).unwrap().lints.clone()
    }

    #[test]
    fn arrow_on_node_tag_receiver_is_silent() {
        // P16.6 — `n->name` where `n: node<Foo>` is the canonical OK
        // shape: `node` is a node-tag, the lint should not fire.
        let diags = project_lints(
            r#"
type Foo {
    name: String;
}

fn f() {
    var n = node<Foo> { Foo { name: "x" } };
    var s = n->name;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "arrow-on-non-deref"),
            "node<T>->field should not flag arrow-on-non-deref: {diags:?}"
        );
    }

    #[test]
    fn arrow_on_primitive_receiver_errors() {
        // P16.6 — `s->size` where `s: String` mirrors the runtime's
        // "arrow operator cannot be applied on String" rejection.
        let diags = project_lints(
            r#"
fn f() {
    var s: String = "hello";
    var n = s->size;
}
"#,
        );
        let hits: Vec<&LintDiagnostic> = diags
            .iter()
            .filter(|d| d.rule == "arrow-on-non-deref")
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one arrow-on-non-deref hit on `s->size`, got {diags:?}"
        );
        assert_eq!(hits[0].severity, LintSeverity::Error);
        assert!(
            hits[0].message.contains("String"),
            "expected the receiver type to surface in the message: {}",
            hits[0].message
        );
    }

    #[test]
    fn arrow_on_user_type_without_deref_errors() {
        // Plain user type without `@deref` — `b->whatever` should
        // surface `arrow-on-non-deref`.
        let diags = project_lints(
            r#"
type Box {
    inner: String;
}

fn f() {
    var b = Box { inner: "x" };
    var x = b->inner;
}
"#,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.rule == "arrow-on-non-deref" && d.message.contains("Box")),
            "expected arrow-on-non-deref on Box receiver: {diags:?}"
        );
    }

    #[test]
    fn arrow_on_deref_annotated_user_type_is_silent() {
        // `@deref("inner")` means the type opts into `->` semantics in
        // the type system. Lint should let it through even though the
        // runtime might still reject non-native bearers — we mirror
        // the *spec* the analyzer is asked to enforce.
        let diags = project_lints(
            r#"
@deref("inner")
type Box {
    inner: String;
}

fn f() {
    var b = Box { inner: "x" };
    var x = b->inner;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "arrow-on-non-deref"),
            "@deref(...) should suppress arrow-on-non-deref: {diags:?}"
        );
    }

    #[test]
    fn arrow_on_any_receiver_is_silent() {
        // Conservative: when the receiver's type is `any` (no concrete
        // head name) we skip the lint rather than firing on every
        // un-typed use.
        let diags = project_lints(
            r#"
fn pick(): any { return 1; }

fn f() {
    var x = pick();
    var y = x->whatever;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "arrow-on-non-deref"),
            "any-typed receivers should not flag: {diags:?}"
        );
    }
}
