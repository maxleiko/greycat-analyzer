//! Linter rules.
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
//! ( placeholder).

use std::ops::Range;

use rustc_hash::FxHashMap;

use greycat_analyzer_core::{SymbolTable, TypeArena, TypeId, TypeKind};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{
    BinOp, BinaryExpr, Decl, Expr, FnDecl, Ident, MemberExpr, OffsetExpr, PropertyName, Stmt,
    TypeDecl, UnaryExpr, UnaryOp,
};

use crate::analyzer::AnalysisResult;
use crate::directives::Directives;
use crate::resolver::{Definition, Resolutions};
use crate::stdlib::ProjectIndex;
use crate::well_known::DeclRegistry;

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
    // P24.5
    /// Optional editor presentation tag. Editors that
    /// honor [`DiagTag::Unnecessary`] (LSP `DiagnosticTag::UNNECESSARY`,
    /// VS Code / Helix / Neovim) dim the source span — the right
    /// surface for "this code does nothing" findings (`unreachable`,
    /// `unused-*`, `redundant-*`). The CLI ignores tags.
    pub tag: Option<DiagTag>,
}

/// Editor presentation hint for a [`LintDiagnostic`]. Maps to LSP
/// `DiagnosticTag` at the server boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagTag {
    /// "Unused / unreachable / dead code." Editors typically render
    /// the span dimmed / faded.
    Unnecessary,
    /// "Deprecated." Editors typically render the span with a
    /// strikethrough. Reserved for a future `deprecated` lint
    /// nothing emits this today.
    #[allow(dead_code)]
    Deprecated,
}

/// Per-rule lookup for the [`DiagTag`] every emission site should set.
/// Centralized so a new "this code does nothing" rule wires the tag
/// just by adding an entry here, rather than touching every emit site.
pub fn default_tag_for(rule: &str) -> Option<DiagTag> {
    match rule {
        "unreachable"
        | "unused-local"
        | "unused-param"
        | "unused-catch-param"
        | "unused-decl"
        | "unused-suppression"
        | "redundant-nullable-access"
        | "redundant-non-null-assertion"
        | "redundant-coalesce"
        | "redundant-semicolon"
        | "no-breakpoint"
        | "conflicting-lint-pragma" => Some(DiagTag::Unnecessary),
        _ => None,
    }
}

/// Trait every lint rule implements. Impls walk `hir` / `res` and emit
/// findings via [`LintCx::emit`], which routes through the active
/// [`crate::directives::Directives`] so rule-specific suppressions
/// (`// gcl-lint-off …`) silence the diagnostic before it lands in the
/// output vector.
pub trait LintRule {
    fn name(&self) -> &'static str;
    fn check(&self, cx: &mut LintCx<'_>);
}

/// Lint context — borrowed by [`LintRule::check`]. Owns the diagnostic
/// sink + the active directive set so per-rule suppressions
/// (`// gcl-lint-off …`) can intercept emissions.
///
/// `bypass_suppressions = true` re-emits every diagnostic the rules
/// produce, even when a suppression covers them — drives the CLI's
/// `--no-suppressions` flag.
pub struct LintCx<'a> {
    pub hir: &'a Hir,
    pub res: &'a Resolutions,
    // FIXME(symbol-migration): added to plumb the symbol table through
    // the lint passes — `Ident` no longer carries `text`, so resolving
    // an ident back to its source string requires the project-wide
    // `SymbolTable`. Set to `&SymbolTable::default()` by per-file
    // callers that don't go through `ProjectAnalysis`.
    pub symbols: &'a SymbolTable,
    pub directives: Option<&'a mut Directives>,
    pub bypass_suppressions: bool,
    out: &'a mut Vec<LintDiagnostic>,
}

impl<'a> LintCx<'a> {
    pub fn new(
        hir: &'a Hir,
        res: &'a Resolutions,
        symbols: &'a SymbolTable,
        directives: Option<&'a mut Directives>,
        bypass_suppressions: bool,
        out: &'a mut Vec<LintDiagnostic>,
    ) -> Self {
        Self {
            hir,
            res,
            symbols,
            directives,
            bypass_suppressions,
            out,
        }
    }

    /// Push `diag` into the output vector unless an active directive
    /// suppresses it (and `bypass_suppressions` is off). Auto-fills
    /// the [`DiagTag`] field via [`default_tag_for`] when the rule
    /// hasn't already set one — saves every per-rule call site from
    /// having to remember to set it.
    pub fn emit(&mut self, mut diag: LintDiagnostic) {
        if !self.bypass_suppressions
            && let Some(dirs) = self.directives.as_deref_mut()
            && dirs.suppresses_lint(diag.byte_range.start, diag.rule)
        {
            return;
        }
        if diag.tag.is_none() {
            diag.tag = default_tag_for(diag.rule);
        }
        self.out.push(diag);
    }
}

/// One row of the lint registry. Keeps the rule's name, severity, and
/// one-line summary together in one place so `lint --list-rules`
/// and the LSP's directive-completion read from the same table.
///
/// `default_enabled` controls whether the rule fires on a vanilla
/// `lint` run. Most rules default `true`. Advisory rules — like
/// `no-breakpoint`, which would silently delete debug aids on a generic
/// `lint --fix` if it fired by default — default `false` and require
/// an opt-in (CLI `--on=<rule>`, future LSP config) to surface.
#[derive(Debug, Clone, Copy)]
pub struct LintRuleInfo {
    pub name: &'static str,
    pub summary: &'static str,
    pub default_enabled: bool,
}

/// `true` if `rule` ships enabled by default. Unknown rules (typo /
/// retired) return `false` — fail-closed.
pub fn is_rule_default_enabled(rule: &str) -> bool {
    LINT_RULES
        .iter()
        .find(|r| r.name == rule)
        .map(|r| r.default_enabled)
        .unwrap_or(false)
}

/// Default-enabled rule registry entry. Keeps [`LINT_RULES`] readable —
/// 90%+ of rules ship on, so the field would just be repetitive noise
/// at the literal site.
const fn rule(name: &'static str, summary: &'static str) -> LintRuleInfo {
    LintRuleInfo {
        name,
        summary,
        default_enabled: true,
    }
}

/// Advisory rule that defaults to off — caller opts in via CLI
/// `--on=<rule>` (or future LSP config). Used for rules that would
/// silently break user intent if they fired on a vanilla `lint --fix`
/// (e.g. `no-breakpoint` would delete debug aids).
const fn advisory_rule(name: &'static str, summary: &'static str) -> LintRuleInfo {
    LintRuleInfo {
        name,
        summary,
        default_enabled: false,
    }
}

/// Project-wide registry of every lint rule the analyzer can emit.
/// Includes both the pure-HIR rules (driven through [`run_lints`]) and
/// the typed lints driven from the project pipeline (`arrow-on-non-deref`,
/// the `nullability` family, `infer-return-type`).
pub const LINT_RULES: &[LintRuleInfo] = &[
    rule(
        "unused-local",
        "warn when a `var name = …;` local is bound but never read",
    ),
    rule(
        "unused-param",
        "warn when a function parameter is never read in its body",
    ),
    rule(
        "unused-decl",
        "warn when a top-level `private` decl is never referenced",
    ),
    rule(
        "duplicate-decl",
        "error when two top-level decls share a name",
    ),
    rule(
        "modvar-must-be-node-tag",
        "module variable type must be a node tag (`node` / `nodeTime` / …)",
    ),
    rule(
        "modvar-node-cannot-be-nullable",
        "module-variable nodes are auto-initialized — drop the trailing `?`",
    ),
    rule(
        "modvar-node-inner-must-be-nullable",
        "`node<T>` requires a nullable inner type — use `node<T?>`",
    ),
    rule(
        "arrow-on-non-deref",
        "`->` requires a node-tag or `@deref` receiver",
    ),
    rule(
        "possibly-null",
        "warn when `.` / `->` / `[…]` is used on a possibly-null receiver",
    ),
    rule(
        "redundant-nullable-access",
        "warn when `?.` / `?->` / `?[…]` is used on a non-nullable receiver",
    ),
    rule(
        "redundant-non-null-assertion",
        "warn when `!!` is used on an already-non-nullable expression",
    ),
    rule(
        "redundant-coalesce",
        "warn when `??` is used on an already-non-nullable left operand",
    ),
    rule(
        "infer-return-type",
        "hint when a fn's return type can be inferred from its body",
    ),
    rule(
        "unreachable",
        "hint when a statement is unreachable (after a divergent prior statement, \
         or the trailing `else` of an exhaustive enum chain)",
    ),
    rule(
        "non-exhaustive",
        "warn when an `if (x == E::A) … else if (x == E::B) …` chain over an enum \
         doesn't cover every variant (and has no catch-all final `else`)",
    ),
    rule(
        "unused-suppression",
        "flag a `// gcl-lint-off…` directive whose rule didn't suppress anything",
    ),
    rule(
        "unknown-suppression-rule",
        "flag a `// gcl-lint-off…` directive that names an unknown rule",
    ),
    rule(
        "empty-suppression",
        "flag a `// gcl-lint-off…` directive with an empty rule list",
    ),
    rule(
        "unbalanced-fmt-off",
        "flag a `// gcl-fmt-off` with no matching `gcl-fmt-on`",
    ),
    rule(
        "unbalanced-lint-off",
        "flag a `// gcl-lint-off …` with no matching `gcl-lint-on`",
    ),
    rule(
        "catch-empty-parens",
        "error on `catch ()` — drop the empty parens (`catch { … }` is the no-binding form)",
    ),
    rule(
        "unused-catch-param",
        "warn when `catch (e) { … }` binds `e` but never reads it — auto-fix drops `(e)` so the form becomes `catch { … }`",
    ),
    rule(
        "redundant-semicolon",
        "error on a stray `;` after a fn or method body (`fn f() {};`) — the runtime rejects it; auto-fix removes the `;`",
    ),
    rule(
        "conflicting-lint-pragma",
        "flag a `@lint_on(\"…\")` / `@lint_off(\"…\")` pair that names the same rule in the same module \
         — `@lint_off` wins; the other pragma is dead",
    ),
    rule(
        "lint-pragma-outside-entrypoint",
        "flag a `@lint_off(\"…\")` / `@lint_on(\"…\")` pragma in a non-entrypoint module — \
         project-wide lint policy belongs in `project.gcl`; auto-fix (P40.6) moves the pragma there",
    ),
    advisory_rule(
        "no-breakpoint",
        "warn on `breakpoint;` left in committed code — pauses the GreyCat worker for \
         debugging; auto-fix deletes the statement. Off by default — enable with \
         `lint --on=no-breakpoint`.",
    ),
];

/// Run every registered HIR-only rule in order and return the merged
/// findings. Suppressions are honored when `directives` is `Some(_)`.
pub fn run_lints(hir: &Hir, res: &Resolutions, symbols: &SymbolTable) -> Vec<LintDiagnostic> {
    let mut out = Vec::new();
    let mut cx = LintCx::new(hir, res, symbols, None, false, &mut out);
    for rule in default_rules() {
        rule.check(&mut cx);
    }
    out
}

/// Same as [`run_lints`] but consults `directives` to suppress rules
/// listed in `// gcl-lint-off …` comments. `bypass_suppressions = true`
/// re-emits everything (drives `lint --no-suppressions`).
pub fn run_lints_with_directives(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    directives: &mut Directives,
    bypass_suppressions: bool,
) -> Vec<LintDiagnostic> {
    let mut out = Vec::new();
    let mut cx = LintCx::new(
        hir,
        res,
        symbols,
        Some(directives),
        bypass_suppressions,
        &mut out,
    );
    for rule in default_rules() {
        rule.check(&mut cx);
    }
    out
}

/// Walk `directives.lint_suppressions` and emit `unused-suppression`
/// diagnostics for every (suppression × rule) pair that didn't actually
/// drop a diagnostic. Per-rule granularity: a `// gcl-lint-next-off A B
/// C` whose A fired but B / C didn't surfaces two diagnostics, one per
/// dead rule.
///
/// `unused-suppression` itself is suppressible only via
/// `// gcl-lint-file-off unused-suppression` (narrower scopes would be
/// circular). Folds findings directly into `out`.
pub fn lint_unused_suppressions(directives: &mut Directives, out: &mut Vec<LintDiagnostic>) {
    // Two-phase: collect first so we can let `Directives::suppresses_lint`
    // mutate state without re-borrowing the suppression list.
    let mut emissions: Vec<LintDiagnostic> = Vec::new();
    for s in &directives.lint_suppressions {
        for entry in &s.rules {
            if s.used_rules.contains(&entry.name) {
                continue;
            }
            // Don't flag the synthetic suppressions that the directive
            // parser itself emits diagnostics for (`unknown-suppression-
            // rule`, `empty-suppression`) — those have no chance of
            // suppressing anything by construction.
            if matches!(
                entry.name.as_str(),
                "unknown-suppression-rule"
                    | "empty-suppression"
                    | "unbalanced-fmt-off"
                    | "unbalanced-lint-off"
            ) {
                continue;
            }
            // Per-rule diagnostic placement: point at the rule word
            // inside the directive comment, not the whole comment line.
            // So `// gcl-lint-next-off A B C` where only A fires gets
            // two diagnostics — one underlining "B", one underlining
            // "C".
            emissions.push(LintDiagnostic {
                rule: "unused-suppression",
                severity: LintSeverity::Warning,
                message: format!("unused suppression for `{}`", entry.name),
                byte_range: entry.byte_range.clone(),
                tag: None,
            });
        }
    }
    for d in emissions {
        // Honor `gcl-lint-file-off unused-suppression` (the only valid
        // way to silence this rule per spec).
        if directives.suppresses_lint(d.byte_range.start, d.rule) {
            continue;
        }
        out.push(d);
    }
}

/// CST walker for `catch ()` — empty catch-parens. The grammar accepts
/// `catch ()` so partial mid-edit text doesn't surface as an `ERROR`
/// node, but the shape carries no semantic information (no ident is
/// bound) and is purely transient. Emit an error with a quickfix that
/// deletes the parens.
///
/// The diagnostic's byte range covers the `(...)` (inclusive of any
/// whitespace between `catch` and `(` that the quickfix should also
/// delete).
pub fn lint_catch_empty_parens(
    text: &str,
    root: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    directives: &mut Directives,
    bypass_suppressions: bool,
    out: &mut Vec<LintDiagnostic>,
) {
    use greycat_analyzer_syntax::tree_sitter::Node;
    fn walk(text: &str, node: Node<'_>, hits: &mut Vec<std::ops::Range<usize>>) {
        if node.kind() == "try_stmt" && node.child_by_field_name("error_param").is_none() {
            // Find the anonymous `(` and `)` direct children. With the
            // P-current grammar the parens are inlined siblings of the
            // try_stmt's named children — they only exist when the user
            // typed `catch ()`.
            let mut cur = node.walk();
            let mut open: Option<usize> = None;
            let mut close: Option<usize> = None;
            for ch in node.children(&mut cur) {
                if ch.is_named() {
                    continue;
                }
                let s = &text[ch.byte_range()];
                if s == "(" && open.is_none() {
                    open = Some(ch.start_byte());
                } else if s == ")" && open.is_some() {
                    close = Some(ch.end_byte());
                }
            }
            if let (Some(start), Some(end)) = (open, close) {
                hits.push(start..end);
            }
        }
        let mut cur = node.walk();
        for ch in node.children(&mut cur) {
            walk(text, ch, hits);
        }
    }
    let mut hits: Vec<std::ops::Range<usize>> = Vec::new();
    walk(text, root, &mut hits);
    for byte_range in hits {
        if !bypass_suppressions
            && directives.suppresses_lint(byte_range.start, "catch-empty-parens")
        {
            continue;
        }
        out.push(LintDiagnostic {
            rule: "catch-empty-parens",
            severity: LintSeverity::Error,
            message: "empty catch parens — drop them (`catch { … }` is the no-binding form)"
                .to_string(),
            byte_range,
            tag: None,
        });
    }
}

/// Flag every `block_trailing_semi` node — a stray `;` after a fn or
/// method body's closing `}`. The grammar permissively accepts the
/// shape so a `};` doesn't open a recovery span that swallows the
/// rest of the surrounding type / module, but the runtime rejects
/// it; this lint is the analyzer-side strict check that pulls the
/// scope back to a single-token diagnostic with a quickfix removing
/// the offending range.
pub fn lint_redundant_semicolon(
    text: &str,
    root: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    directives: &mut Directives,
    bypass_suppressions: bool,
    out: &mut Vec<LintDiagnostic>,
) {
    use greycat_analyzer_syntax::tree_sitter::Node;
    fn walk(node: Node<'_>, hits: &mut Vec<std::ops::Range<usize>>) {
        if node.kind() == "block_trailing_semi" {
            hits.push(node.byte_range());
        }
        let mut cur = node.walk();
        for ch in node.children(&mut cur) {
            walk(ch, hits);
        }
    }
    let mut hits: Vec<std::ops::Range<usize>> = Vec::new();
    walk(root, &mut hits);
    for byte_range in hits {
        if !bypass_suppressions
            && directives.suppresses_lint(byte_range.start, "redundant-semicolon")
        {
            continue;
        }
        let message = if text
            .get(byte_range.clone())
            .is_some_and(|s| s.contains(";;"))
        {
            "redundant `;` after fn / method body — drop them".to_string()
        } else {
            "redundant `;` after fn / method body — drop it".to_string()
        };
        out.push(LintDiagnostic {
            rule: "redundant-semicolon",
            severity: LintSeverity::Error,
            message,
            byte_range,
            tag: None,
        });
    }
}

// P37.7
/// Flag every `breakpoint_stmt` node — `breakpoint;` pauses the GreyCat
/// worker for debugging. The keyword is real and the runtime honors it,
/// but committed `breakpoint;` stalls production runs. Same shape of
/// mistake as a leftover Rust `dbg!()` or JS `debugger;`. Severity
/// Warning + `UNNECESSARY` tag so editors dim the span; users can opt
/// out per-site via `// gcl-lint-off no-breakpoint`.
pub fn lint_no_breakpoint(
    root: greycat_analyzer_syntax::tree_sitter::Node<'_>,
    directives: &mut Directives,
    bypass_suppressions: bool,
    out: &mut Vec<LintDiagnostic>,
) {
    use greycat_analyzer_syntax::tree_sitter::Node;
    fn walk(node: Node<'_>, hits: &mut Vec<std::ops::Range<usize>>) {
        if node.kind() == "breakpoint_stmt" {
            hits.push(node.byte_range());
        }
        let mut cur = node.walk();
        for ch in node.children(&mut cur) {
            walk(ch, hits);
        }
    }
    let mut hits: Vec<std::ops::Range<usize>> = Vec::new();
    walk(root, &mut hits);
    for byte_range in hits {
        emit_typed(
            out,
            Some(directives),
            bypass_suppressions,
            LintDiagnostic {
                rule: "no-breakpoint",
                severity: LintSeverity::Warning,
                message: "`breakpoint;` left in committed code — it pauses the worker for \
                          debugging (suppress with `// gcl-lint-off no-breakpoint` if intentional)"
                    .to_string(),
                byte_range,
                tag: None,
            },
        );
    }
}

fn default_rules() -> Vec<Box<dyn LintRule>> {
    vec![
        Box::new(UnusedLocal),
        Box::new(UnusedParam),
        Box::new(UnusedCatchParam),
        Box::new(UnusedDecl),
        Box::new(DuplicateDecl),
        Box::new(ModVarShape),
    ]
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
    visit_for_locals(hir, res, symbols, body, out, rule);
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

fn visit_block_for_locals(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    block: &greycat_analyzer_hir::types::BlockStmt,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    for s in &block.stmts {
        visit_for_locals(hir, res, symbols, *s, out, rule);
    }
}

fn visit_for_locals(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
    stmt_id: Idx<Stmt>,
    out: &mut Vec<LintDiagnostic>,
    rule: &'static str,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => visit_block_for_locals(hir, res, symbols, b, out, rule),
        Stmt::Var(v) => {
            let ident = &hir.idents[v.name];
            let name = &symbols[ident.symbol];
            if name.starts_with('_') {
                return;
            }
            let used = res.uses.values().any(|d| match d {
                Definition::Local(name) => *name == v.name,
                _ => false,
            });
            if !used {
                out.push(LintDiagnostic {
                    rule,
                    severity: LintSeverity::Warning,
                    message: format!("unused local `{name}`"),
                    byte_range: ident.byte_range.clone(),
                    tag: None,
                });
            }
        }
        Stmt::If(i) => {
            visit_block_for_locals(hir, res, symbols, &i.then_branch, out, rule);
            if let Some(eb) = i.else_branch {
                visit_for_locals(hir, res, symbols, eb, out, rule);
            }
        }
        Stmt::While(w) => visit_block_for_locals(hir, res, symbols, &w.body, out, rule),
        Stmt::DoWhile(w) => visit_block_for_locals(hir, res, symbols, &w.body, out, rule),
        Stmt::For(f) => visit_block_for_locals(hir, res, symbols, &f.body, out, rule),
        Stmt::ForIn(f) => visit_block_for_locals(hir, res, symbols, &f.body, out, rule),
        Stmt::Try(t) => {
            visit_block_for_locals(hir, res, symbols, &t.try_block, out, rule);
            visit_block_for_locals(hir, res, symbols, &t.catch_block, out, rule);
        }
        Stmt::At(a) => visit_block_for_locals(hir, res, symbols, &a.block, out, rule),
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

fn check_fn_params(
    hir: &Hir,
    res: &Resolutions,
    symbols: &SymbolTable,
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
    use greycat_analyzer_hir::types::BlockStmt;
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

// =============================================================================
// Rule: unused-decl
// =============================================================================

// P6.7
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
        if !modifiers.private {
            continue;
        }
        if exposes_runtime(modifiers) {
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

    fn check(&self, cx: &mut LintCx<'_>) {
        let Some(module) = cx.hir.module.as_ref() else {
            return;
        };
        let mut seen: FxHashMap<greycat_analyzer_core::Symbol, ()> = FxHashMap::default();
        let mut candidates: Vec<LintDiagnostic> = Vec::new();
        for decl_id in &module.decls {
            let Some(name_id) = cx.hir.decls[*decl_id].name() else {
                continue;
            };
            if matches!(&cx.hir.decls[*decl_id], Decl::Pragma(_)) {
                continue;
            }
            let ident = &cx.hir.idents[name_id];
            if seen.insert(ident.symbol, ()).is_some() {
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

/// Walk every `Expr::Arrow` and emit an error when the receiver's type
/// is not declared with `@deref("methodName")` in the
/// `ProjectIndex::type_flags` table. Mirrors the GreyCat runtime's
/// "cannot deref" rejection — caught at edit time rather than at run.
/// The runtime types `node<T>` / `nodeTime<T>` / `nodeList<T>` /
/// `nodeGeo<T>` carry `@deref("resolve")` on their `lib/std/core.gcl`
/// decl, so once the stdlib is ingested they participate just like
/// any user-declared `@deref`-annotated type — no hard-coded
/// name-keyed allowlist needed.
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
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
) {
    lint_arrow_on_non_deref_inner(hir, analysis, arena, index, decl_registry, out, None, false);
}

/// Directive-aware variant of [`lint_arrow_on_non_deref`]. Drops
/// suppressed emissions before they land in `out`.
pub fn lint_arrow_on_non_deref_with_directives(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
    directives: &mut Directives,
    bypass_suppressions: bool,
) {
    lint_arrow_on_non_deref_inner(
        hir,
        analysis,
        arena,
        index,
        decl_registry,
        out,
        Some(directives),
        bypass_suppressions,
    );
}

fn lint_arrow_on_non_deref_inner(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    index: &ProjectIndex,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
    mut directives: Option<&mut Directives>,
    bypass_suppressions: bool,
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
        let head = receiver_head_name(arena, decl_registry, &index.symbols, recv_ty);
        let Some(name) = head else {
            // Conservative: receiver is `any` / lambda / tuple / etc. —
            // no head name to classify. Skip.
            continue;
        };
        if index
            .type_flags_for(&name)
            .is_some_and(|f| f.deref.is_some())
        {
            continue;
        }
        let _ = expr_id;
        emit_typed(
            out,
            directives.as_deref_mut(),
            bypass_suppressions,
            LintDiagnostic {
                rule: "arrow-on-non-deref",
                severity: LintSeverity::Error,
                message: format!("`->` requires a node-tag or `@deref` receiver, got `{name}`"),
                byte_range: byte_range.clone(),
                tag: None,
            },
        );
    }
}

/// Emit-via-directives helper for the typed-lint free functions. They
/// don't go through [`LintCx`] because they take a richer set of
/// arguments (TypeArena, ProjectIndex, AnalysisResult) and have nothing
/// useful to do with `LintCx::hir`/`res`'s simpler signature. Same
/// auto-tag behavior as [`LintCx::emit`].
fn emit_typed(
    out: &mut Vec<LintDiagnostic>,
    directives: Option<&mut Directives>,
    bypass_suppressions: bool,
    mut diag: LintDiagnostic,
) {
    if !bypass_suppressions
        && let Some(dirs) = directives
        && dirs.suppresses_lint(diag.byte_range.start, diag.rule)
    {
        return;
    }
    if diag.tag.is_none() {
        diag.tag = default_tag_for(diag.rule);
    }
    out.push(diag);
}

/// Extract the head name of `recv_ty` for `arrow-on-non-deref` dispatch.
/// Strips top-level nullability and reduces `Type` / `Generic` /
/// `Primitive` to their canonical name. Returns `None` for shapes the
/// lint conservatively skips (any / never / null / lambda / tuple /
/// anonymous / union / enum / generic-param).
fn receiver_head_name(
    arena: &TypeArena,
    decl_registry: &DeclRegistry,
    symbols: &SymbolTable,
    ty: TypeId,
) -> Option<String> {
    let t = arena.get(ty);
    let decl_name = |d| decl_registry.name(d).map(|sym| symbols[sym].to_string());
    match &t.kind {
        // P35.7 — `TypeKind::Type(handle)` / `Generic` recover
        // their decl name via the project's `DeclRegistry`.
        TypeKind::Type(decl) => decl_name(*decl),
        TypeKind::Generic { decl, .. } => decl_name(*decl),
        TypeKind::Primitive(p) => Some(p.name().to_string()),
        _ => None,
    }
}

// =============================================================================
// Rule: modvar-shape  (P19.18 — module variable type constraints)
// =============================================================================
//
// Module variables (`var` at top level) are GreyCat's persistent-store
// roots and the runtime constrains their type tightly. The TS reference
// emits three distinct errors (`Module variable type must be one of …`,
// `Nodes are automatically initialized by GreyCat, they cannot be null`,
// and the per-node-collection inner-shape rules). We mirror them here
// as a pure-HIR rule (no typing context needed — the type *ref* spelling
// is what's constrained).

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

// =============================================================================
// Rule family: nullability hygiene  (P19.17 — typed lint)
// =============================================================================
//
// Four sibling rules fueled by the analyzer's `expr_types` table:
//
//   - `possibly-null` — Member / Arrow / Offset access on a still-
//     nullable receiver without an opt-in marker (`?.` / `?->` / `?[`).
//     Mirrors the TS reference's `'x' is possibly 'null'` warning.
//   - `redundant-nullable-access` — `?.` / `?->` / `?[` written when the
//     receiver is already known non-null. The marker is dead weight.
//   - `redundant-non-null-assertion` — `!!x` where `x` is already
//     non-null. The assertion is dead weight.
//   - `redundant-coalesce` — `lhs ?? rhs` where `lhs` is already
//     non-null. The fallback can never fire, drop the `??` clause.
//
// All four read the *narrowed* receiver/operand type from
// `analysis.expr_types` (the analyzer writes narrows in-line during S12
// body walking, so a binding under `if (x != null) { … }` already has
// its stripped type recorded for that visit). They skip `Any` and
// `Null` — those are too noisy to warn on (every untyped expression
// would flag) and degenerate, respectively.

// =============================================================================
// Rule: infer-return-type  (P19.20 — typed lint)
// =============================================================================
//
// Hint when a `fn` has no declared return type but the analyzer could
// infer one from its body. Mirrors the TS reference's
// `Return type can be inferred as 'X' (fix available)` hint. Pure HIR +
// `expr_types` walk: pulls the last `return value;` per fn body and
// reads its settled type. Skips `any` / `never` (uninformative) and
// fns with `native` / `abstract` modifiers (no body to infer from).

/// Runs from the project pipeline alongside the other typed lints.
/// Walks every top-level fn and every type-method fn; any `Decl::Fn`
/// without a declared `return_type` whose body terminates in
/// `return e;` with a settled, informative type gets a HINT.
pub fn lint_inferred_return_type(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    symbols: &SymbolTable,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
) {
    lint_inferred_return_type_inner(
        hir,
        analysis,
        arena,
        symbols,
        decl_registry,
        out,
        None,
        false,
    );
}

/// Directive-aware variant of [`lint_inferred_return_type`].
pub fn lint_inferred_return_type_with_directives(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    symbols: &SymbolTable,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
    directives: &mut Directives,
    bypass_suppressions: bool,
) {
    lint_inferred_return_type_inner(
        hir,
        analysis,
        arena,
        symbols,
        decl_registry,
        out,
        Some(directives),
        bypass_suppressions,
    );
}

fn lint_inferred_return_type_inner(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    symbols: &SymbolTable,
    decl_registry: &DeclRegistry,
    out: &mut Vec<LintDiagnostic>,
    mut directives: Option<&mut Directives>,
    bypass_suppressions: bool,
) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    for decl_id in &module.decls {
        match &hir.decls[*decl_id] {
            Decl::Fn(fnd) => check_fn_inferred_return(
                hir,
                analysis,
                arena,
                symbols,
                decl_registry,
                fnd,
                out,
                directives.as_deref_mut(),
                bypass_suppressions,
            ),
            Decl::Type(td) => {
                for m_id in &td.methods {
                    if let Decl::Fn(fnd) = &hir.decls[*m_id] {
                        check_fn_inferred_return(
                            hir,
                            analysis,
                            arena,
                            symbols,
                            decl_registry,
                            fnd,
                            out,
                            directives.as_deref_mut(),
                            bypass_suppressions,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn check_fn_inferred_return(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    symbols: &SymbolTable,
    decl_registry: &DeclRegistry,
    fnd: &FnDecl,
    out: &mut Vec<LintDiagnostic>,
    directives: Option<&mut Directives>,
    bypass_suppressions: bool,
) {
    if fnd.return_type.is_some() {
        return;
    }
    if fnd.modifiers.native || fnd.modifiers.abstract_ {
        return;
    }
    let Some(body) = fnd.body else {
        return;
    };
    let Some(ret_ty) = inferred_return_from_body(hir, analysis, body) else {
        return;
    };
    let kind = &arena.get(ret_ty).kind;
    if matches!(kind, TypeKind::Any | TypeKind::Never) {
        return;
    }
    let display = crate::project::display_type(arena, decl_registry, symbols, ret_ty);
    let name = &hir.idents[fnd.name];
    emit_typed(
        out,
        directives,
        bypass_suppressions,
        LintDiagnostic {
            rule: "infer-return-type",
            severity: LintSeverity::Hint,
            message: format!("return type can be inferred as `{display}`"),
            byte_range: name.byte_range.clone(),
            tag: None,
        },
    );
}

/// Walk a fn body's terminal block looking for the last `return e;`
/// whose value type was recorded in `expr_types`. Mirrors the inlay-
/// hint helper in `capabilities`. Misses fns whose every return is
/// in a nested branch (acceptable — we only suggest when the inference
/// is unambiguous from the surface).
fn inferred_return_from_body(
    hir: &Hir,
    analysis: &AnalysisResult,
    body: Idx<Stmt>,
) -> Option<TypeId> {
    let Stmt::Block(block) = &hir.stmts[body] else {
        return None;
    };
    for s in block.stmts.iter().rev() {
        if let Stmt::Return(Some(e)) = &hir.stmts[*s] {
            return analysis.expr_types.get(e).copied();
        }
    }
    None
}

/// True when `expr_id` is downstream of a `?.` / `?->` / `?[` somewhere
/// in its receiver chain. Such positions are *runtime-safe* even when
/// the expression's type is nullable: optional chaining short-circuits
/// the entire suffix when the upstream marker shorts, so `.x` on a
/// chain-nullable receiver isn't evaluated when the upstream `?.` did
/// fire. The typing pipeline still propagates nullability for these
/// (the *expression* really is nullable), but the `possibly-null` lint
/// must skip them — the user already opted into the safe form upstream.
///
/// Walks Member / Arrow / Offset / Call / Paren receivers; everything
/// else terminates the walk (we hit the chain root).
pub fn chain_has_upstream_nullsafe(hir: &Hir, expr_id: Idx<Expr>) -> bool {
    let mut cur = expr_id;
    loop {
        match &hir.exprs[cur] {
            Expr::Member(m) | Expr::Arrow(m) => {
                if m.pre_optional || m.post_optional {
                    return true;
                }
                cur = m.receiver;
            }
            Expr::Offset(o) => {
                if o.pre_optional || o.post_optional {
                    return true;
                }
                cur = o.receiver;
            }
            Expr::Call(c) => {
                cur = c.callee;
            }
            Expr::Paren(inner, _) => {
                cur = *inner;
            }
            _ => return false,
        }
    }
}

/// Drives the four nullability-hygiene rules in a single expression
/// walk. Wired into the project pipeline next to
/// [`lint_arrow_on_non_deref`] so it sees the same post-S12 typing
/// state. Per-rule emission is short-circuited when the receiver type
/// is `Any` / `Null` (conservative skip) or absent from `expr_types`
/// (the visit didn't reach this expression — usually because it's
/// inside dead code or an `Unsupported` lowering).
///
/// **Intentionally NOT covered: `*n` (`Expr::Unary { op: Deref }`)**
/// the GreyCat runtime's deref operator handles the null-receiver case
/// gracefully (returns `null` for a null node-tag), so flagging
/// `*nullable_node` as "possibly null" would be a false positive even
/// though the underlying type is nullable. Diverges from the TS
/// reference here on purpose.
pub fn lint_nullability(
    hir: &Hir,
    symbols: &SymbolTable,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    out: &mut Vec<LintDiagnostic>,
) {
    lint_nullability_inner(hir, symbols, analysis, arena, out, None, false);
}

/// Directive-aware variant of [`lint_nullability`].
pub fn lint_nullability_with_directives(
    hir: &Hir,
    symbols: &SymbolTable,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    out: &mut Vec<LintDiagnostic>,
    directives: &mut Directives,
    bypass_suppressions: bool,
) {
    lint_nullability_inner(
        hir,
        symbols,
        analysis,
        arena,
        out,
        Some(directives),
        bypass_suppressions,
    );
}

fn lint_nullability_inner(
    hir: &Hir,
    symbols: &SymbolTable,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    out: &mut Vec<LintDiagnostic>,
    mut directives: Option<&mut Directives>,
    bypass_suppressions: bool,
) {
    for (expr_id, expr) in hir.exprs.iter() {
        match expr {
            Expr::Member(MemberExpr {
                receiver,
                property,
                pre_optional,
                byte_range,
                ..
            })
            | Expr::Arrow(MemberExpr {
                receiver,
                property,
                pre_optional,
                byte_range,
                ..
            }) => {
                let Some(recv_ty) = analysis.expr_types.get(receiver).copied() else {
                    continue;
                };
                let recv = arena.get(recv_ty);
                if matches!(recv.kind, TypeKind::Any | TypeKind::Null) {
                    continue;
                }
                let recv_range = receiver_byte_range(hir, *receiver);
                if recv.nullable && !*pre_optional && !chain_has_upstream_nullsafe(hir, *receiver) {
                    let display = display_receiver(hir, symbols, *receiver);
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "possibly-null",
                            severity: LintSeverity::Warning,
                            message: format!("`{display}` is possibly `null`"),
                            byte_range: recv_range.clone(),
                            tag: None,
                        },
                    );
                } else if !recv.nullable && *pre_optional {
                    let prop_start = hir.idents[property.ident()].byte_range.start;
                    let _ = expr_id;
                    let _ = byte_range;
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "redundant-nullable-access",
                            severity: LintSeverity::Warning,
                            message: "redundant `?` — receiver is non-nullable".into(),
                            byte_range: recv_range.end..prop_start,
                            tag: None,
                        },
                    );
                }
            }
            Expr::Offset(OffsetExpr {
                receiver,
                pre_optional,
                byte_range,
                ..
            }) => {
                let Some(recv_ty) = analysis.expr_types.get(receiver).copied() else {
                    continue;
                };
                let recv = arena.get(recv_ty);
                if matches!(recv.kind, TypeKind::Any | TypeKind::Null) {
                    continue;
                }
                let recv_range = receiver_byte_range(hir, *receiver);
                if recv.nullable && !*pre_optional && !chain_has_upstream_nullsafe(hir, *receiver) {
                    let display = display_receiver(hir, symbols, *receiver);
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "possibly-null",
                            severity: LintSeverity::Warning,
                            message: format!("`{display}` is possibly `null`"),
                            byte_range: recv_range.clone(),
                            tag: None,
                        },
                    );
                } else if !recv.nullable && *pre_optional {
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "redundant-nullable-access",
                            severity: LintSeverity::Warning,
                            message: "redundant `?` — receiver is non-nullable".into(),
                            byte_range: recv_range.end..byte_range.end,
                            tag: None,
                        },
                    );
                }
            }
            Expr::Unary(UnaryExpr {
                op: UnaryOp::NonNullAssert,
                operand,
                byte_range,
            }) => {
                let Some(op_ty) = analysis.expr_types.get(operand).copied() else {
                    continue;
                };
                let opnd = arena.get(op_ty);
                if matches!(opnd.kind, TypeKind::Any | TypeKind::Null) {
                    continue;
                }
                if !opnd.nullable {
                    let opnd_range = receiver_byte_range(hir, *operand);
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "redundant-non-null-assertion",
                            severity: LintSeverity::Warning,
                            message: "redundant `!!` — expression is already non-nullable".into(),
                            byte_range: opnd_range.end..byte_range.end,
                            tag: None,
                        },
                    );
                }
            }
            Expr::Binary(BinaryExpr {
                op: BinOp::Coalesce,
                left,
                byte_range,
                ..
            }) => {
                let Some(lt) = analysis.expr_types.get(left).copied() else {
                    continue;
                };
                let l = arena.get(lt);
                if matches!(l.kind, TypeKind::Any | TypeKind::Null) {
                    continue;
                }
                if !l.nullable {
                    let left_range = receiver_byte_range(hir, *left);
                    emit_typed(
                        out,
                        directives.as_deref_mut(),
                        bypass_suppressions,
                        LintDiagnostic {
                            rule: "redundant-coalesce",
                            severity: LintSeverity::Warning,
                            message: "redundant `??` — left operand is already non-nullable".into(),
                            byte_range: left_range.end..byte_range.end,
                            tag: None,
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// Byte range of an arbitrary expression. Thin wrapper around
/// [`Expr::byte_range`] kept as a named helper for call-site clarity.
fn receiver_byte_range(hir: &Hir, expr_id: Idx<Expr>) -> Range<usize> {
    hir.exprs[expr_id].byte_range()
}

/// Render a receiver expression as quoted source-like text for the
/// `possibly-null` message. Walks Ident / Member / Arrow / Static
/// chains directly. Falls back to `expression` for shapes that don't
/// have a clean textual form (calls, lambdas, casts, …).
/// Render a [`PropertyName`] back to its source-shaped form for use
/// inside diagnostic messages: bareword for `Ident`, quoted for
/// `String`. Mirrors what the user wrote so messages like
/// `` `conf."ssl.location"` is possibly null `` quote the source.
fn display_property(hir: &Hir, symbols: &SymbolTable, property: PropertyName) -> String {
    let text = &symbols[hir.idents[property.ident()].symbol];
    match property {
        PropertyName::Ident(_) => text.to_string(),
        PropertyName::String(_) => format!("\"{text}\""),
    }
}

fn display_receiver(hir: &Hir, symbols: &SymbolTable, expr_id: Idx<Expr>) -> String {
    match &hir.exprs[expr_id] {
        Expr::Ident { name: name_idx, .. } => symbols[hir.idents[*name_idx].symbol].to_string(),
        Expr::This { .. } => "this".into(),
        Expr::Literal(_) | Expr::Null { .. } => "expression".into(),
        Expr::Member(m) => {
            let recv = display_receiver(hir, symbols, m.receiver);
            let prop = display_property(hir, symbols, m.property);
            let q = if m.pre_optional { "?." } else { "." };
            let post = if m.post_optional { "?" } else { "" };
            format!("{recv}{q}{prop}{post}")
        }
        Expr::Arrow(m) => {
            let recv = display_receiver(hir, symbols, m.receiver);
            let prop = display_property(hir, symbols, m.property);
            let q = if m.pre_optional { "?->" } else { "->" };
            let post = if m.post_optional { "?" } else { "" };
            format!("{recv}{q}{prop}{post}")
        }
        Expr::Static(s) => {
            let ty = &hir.type_refs[s.ty];
            let prop = display_property(hir, symbols, s.property);
            let ty_name = &symbols[hir.idents[ty.name].symbol];
            format!("{ty_name}::{prop}")
        }
        Expr::Paren(inner, _) => display_receiver(hir, symbols, *inner),
        _ => "expression".into(),
    }
}

// =============================================================================
// Rule: unreachable  (P24.3 — typed lint)
// =============================================================================
//
// Walk every fn / method body and flag statements that follow a
// divergent prior statement (return / throw / break / continue or a
// recursively-divergent block / if / try / exhaustive-enum chain).
// Also flag the trailing `else { … }` arm of an exhaustive enum chain
// — it's covered by the prior arms and can never be entered.
//
// Severity = Hint. Dead code is rarely a *bug* (more often it's a
// refactor leftover), so the editor's greyed-out treatment via
// `DiagnosticTag::UNNECESSARY` (P24.5) is the right surface — not an
// error or warning.
//
// Per-block, contiguous dead siblings are coalesced into one
// diagnostic whose byte range spans the whole island. P24.4 layers
// outer-island dominance on top so a dead inner block doesn't
// double-flag.

pub fn lint_unreachable(hir: &Hir, analysis: &AnalysisResult, out: &mut Vec<LintDiagnostic>) {
    lint_unreachable_inner(hir, analysis, out, None, false);
}

/// Directive-aware variant.
pub fn lint_unreachable_with_directives(
    hir: &Hir,
    analysis: &AnalysisResult,
    out: &mut Vec<LintDiagnostic>,
    directives: &mut Directives,
    bypass_suppressions: bool,
) {
    lint_unreachable_inner(hir, analysis, out, Some(directives), bypass_suppressions);
}

fn lint_unreachable_inner(
    hir: &Hir,
    analysis: &AnalysisResult,
    out: &mut Vec<LintDiagnostic>,
    mut directives: Option<&mut Directives>,
    bypass_suppressions: bool,
) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    // Two-phase: collect every dead range first (with outer-island
    // dominance applied), then emit. Deferring emission lets P24.4's
    // "skip ranges contained in an already-recorded outer range"
    // post-filter operate cleanly.
    let mut dead_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for decl_id in &module.decls {
        match &hir.decls[*decl_id] {
            Decl::Fn(fnd) => collect_dead_in_fn(hir, analysis, fnd, &mut dead_ranges),
            Decl::Type(td) => {
                for m_id in &td.methods {
                    if let Decl::Fn(fnd) = &hir.decls[*m_id] {
                        collect_dead_in_fn(hir, analysis, fnd, &mut dead_ranges);
                    }
                }
            }
            _ => {}
        }
    }
    // Outer-island dominance: drop any range that's fully contained
    // in another. Sorting by start ascending then end descending
    // makes the "outer covers inner" check linear.
    dead_ranges.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut kept: Vec<std::ops::Range<usize>> = Vec::with_capacity(dead_ranges.len());
    for r in dead_ranges {
        if let Some(last) = kept.last()
            && last.start <= r.start
            && r.end <= last.end
        {
            continue;
        }
        kept.push(r);
    }
    for r in kept {
        emit_typed(
            out,
            directives.as_deref_mut(),
            bypass_suppressions,
            LintDiagnostic {
                rule: "unreachable",
                severity: LintSeverity::Hint,
                message: "unreachable code".into(),
                byte_range: r,
                tag: None,
            },
        );
    }
}

/// Emit a `non-exhaustive` lint for every enum-eq chain the analyzer
/// flagged in pass 2. The analyzer records the findings into
/// [`AnalysisResult::non_exhaustive_findings`] so this rule can ride
/// the standard lint pipeline (`emit_typed` honors `// gcl-lint-off…`
/// directives, `unused-suppression` checks against actual emissions,
/// quickfixes dispatch by `code`).
pub fn lint_non_exhaustive_with_directives(
    analysis: &AnalysisResult,
    out: &mut Vec<LintDiagnostic>,
    directives: &mut Directives,
    bypass_suppressions: bool,
) {
    for finding in &analysis.non_exhaustive_findings {
        let msg = format!(
            "non-exhaustive match over `{}` (missing: {})",
            finding.enum_name,
            finding.missing.join(", "),
        );
        emit_typed(
            out,
            Some(&mut *directives),
            bypass_suppressions,
            LintDiagnostic {
                rule: "non-exhaustive",
                severity: LintSeverity::Warning,
                message: msg,
                byte_range: finding.byte_range.clone(),
                tag: None,
            },
        );
    }
}

fn collect_dead_in_fn(
    hir: &Hir,
    analysis: &AnalysisResult,
    fnd: &FnDecl,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    if fnd.modifiers.native || fnd.modifiers.abstract_ {
        return;
    }
    let Some(body_id) = fnd.body else {
        return;
    };
    if let Stmt::Block(body) = &hir.stmts[body_id] {
        collect_dead_in_block(hir, analysis, body, out);
    }
}

/// Walk a block: find dead siblings (post-divergent), recurse into
/// every statement's nested blocks, and flag dead else arms of
/// exhaustive enum chains.
fn collect_dead_in_block(
    hir: &Hir,
    analysis: &AnalysisResult,
    block: &greycat_analyzer_hir::types::BlockStmt,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    // Pass 1: any statement that follows a divergent sibling becomes
    // dead. Coalesce the contiguous dead suffix into one range.
    let mut dead_start_idx: Option<usize> = None;
    for (i, s) in block.stmts.iter().enumerate() {
        if dead_start_idx.is_none()
            && crate::reachability::stmt_diverges_with_analysis(hir, analysis, *s)
            && i + 1 < block.stmts.len()
        {
            dead_start_idx = Some(i + 1);
            break;
        }
    }
    if let Some(start_idx) = dead_start_idx {
        let first = stmt_byte_range(hir, block.stmts[start_idx]);
        let last = stmt_byte_range(hir, *block.stmts.last().unwrap());
        out.push(first.start..last.end);
    }
    // Pass 2: recurse into every statement's nested blocks (so dead
    // code inside a still-reachable arm is also flagged), but skip
    // statements that already sit inside the dead suffix above —
    // outer-island dominance handles those.
    let dead_from = dead_start_idx.unwrap_or(usize::MAX);
    for (i, s) in block.stmts.iter().enumerate() {
        if i >= dead_from {
            // Already covered by the outer dead range; recursing
            // would double-flag.
            continue;
        }
        collect_dead_in_stmt(hir, analysis, *s, out);
    }
}

fn collect_dead_in_stmt(
    hir: &Hir,
    analysis: &AnalysisResult,
    stmt_id: Idx<Stmt>,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    match &hir.stmts[stmt_id] {
        Stmt::Block(b) => collect_dead_in_block(hir, analysis, b, out),
        Stmt::If(i) => {
            // Trivially-decidable condition: flag the dead branch
            // and skip the regular passes (outer-island dominance
            // would otherwise re-flag inner shapes).
            match analysis.decidable_conditions.get(&stmt_id) {
                Some(false) => {
                    // Condition always false → if-keyword through
                    // end of then-block is dead. The else-branch
                    // (if any) is live; the quickfix unwraps to it.
                    let dead_end = i.then_branch.byte_range.end;
                    out.push(i.byte_range.start..dead_end);
                    if let Some(eb) = i.else_branch {
                        collect_dead_in_stmt(hir, analysis, eb, out);
                    }
                    return;
                }
                Some(true) => {
                    // Condition always true → the else branch (if
                    // any) is dead. Range = else block's byte_range,
                    // matching the existing dead-else shape so
                    // `unreachable_fix` swallows the leading `else `.
                    if let Some(eb) = i.else_branch
                        && let Some(dead) = else_block_range(hir, eb)
                    {
                        out.push(dead);
                    }
                    // Then-branch is live; recurse into it for
                    // nested dead-code inside.
                    collect_dead_in_block(hir, analysis, &i.then_branch, out);
                    return;
                }
                None => {}
            }
            collect_dead_in_block(hir, analysis, &i.then_branch, out);
            // Dead-else flagging: if THIS if is the head of an
            // exhaustive chain AND has a final `else { … }`, the
            // else block is unreachable. Flag it.
            if analysis.exhaustive_enum_chains.contains(&stmt_id)
                && let Some(dead) =
                    crate::reachability::dead_else_range_for_exhaustive_chain(hir, stmt_id)
            {
                out.push(dead);
            }
            if let Some(eb) = i.else_branch {
                collect_dead_in_stmt(hir, analysis, eb, out);
            }
        }
        Stmt::While(w) => {
            // Always-false `while` → whole stmt is dead.
            if analysis.decidable_conditions.get(&stmt_id) == Some(&false) {
                out.push(w.byte_range.clone());
                return;
            }
            collect_dead_in_block(hir, analysis, &w.body, out);
        }
        Stmt::DoWhile(w) => collect_dead_in_block(hir, analysis, &w.body, out),
        Stmt::For(f) => {
            // Always-false C-style `for` → whole stmt is dead. The
            // init-binding's side effect is lost on deletion, but
            // a `for (var i = 0; false; …)` is so suspect that the
            // quickfix's slice-delete is the right default; the
            // user can hoist init manually if needed.
            if analysis.decidable_conditions.get(&stmt_id) == Some(&false) {
                out.push(f.byte_range.clone());
                return;
            }
            collect_dead_in_block(hir, analysis, &f.body, out);
        }
        Stmt::ForIn(f) => collect_dead_in_block(hir, analysis, &f.body, out),
        Stmt::Try(t) => {
            collect_dead_in_block(hir, analysis, &t.try_block, out);
            collect_dead_in_block(hir, analysis, &t.catch_block, out);
        }
        Stmt::At(a) => collect_dead_in_block(hir, analysis, &a.block, out),
        _ => {}
    }
}

/// Byte range of an `else` clause's payload — either the `{ … }`
/// block (the common case) or a nested `if` (the `else if` chain).
/// Used by trivially-decidable-condition dead-flagging to match the
/// existing dead-else range shape `unreachable_fix` already handles.
fn else_block_range(hir: &Hir, eb: Idx<Stmt>) -> Option<std::ops::Range<usize>> {
    match &hir.stmts[eb] {
        Stmt::Block(b) => Some(b.byte_range.clone()),
        Stmt::If(i) => Some(i.byte_range.clone()),
        _ => None,
    }
}

/// Best-effort `byte_range` for an arbitrary statement. Mirrors what
/// quickfix's enclosing-node walker would compute — we use the
/// statement's own byte_range when available, falling back to a
/// recursive lookup for shapes like `Stmt::Expr` that don't carry a
/// span.
fn stmt_byte_range(hir: &Hir, stmt_id: Idx<Stmt>) -> std::ops::Range<usize> {
    match &hir.stmts[stmt_id] {
        Stmt::Block(b) => b.byte_range.clone(),
        Stmt::Var(v) => v.byte_range.clone(),
        Stmt::Assign(a) => a.byte_range.clone(),
        Stmt::If(i) => i.byte_range.clone(),
        Stmt::While(w) => w.byte_range.clone(),
        Stmt::DoWhile(w) => w.byte_range.clone(),
        Stmt::For(f) => f.byte_range.clone(),
        Stmt::ForIn(f) => f.byte_range.clone(),
        Stmt::Try(t) => t.byte_range.clone(),
        Stmt::At(a) => a.byte_range.clone(),
        Stmt::Expr(e) => hir.exprs[*e].byte_range(),
        Stmt::Return(_) | Stmt::Break | Stmt::Continue | Stmt::Breakpoint | Stmt::Throw(_) => {
            // These keyword-only statements don't carry their own
            // byte_range; fall back to the inner expression's span
            // (return/throw) or to a zero-width range (break/continue/
            // breakpoint). The lint never produces a *primary* diagnostic
            // on the divergent shapes (return/throw/break/continue) —
            // they're terminators, not dead — so 0..0 only fires for
            // the rare path where one of those is the *first* dead stmt,
            // which can't happen. `breakpoint` isn't divergent, but it
            // still has no inner expr, so 0..0 is the same fallback.
            match &hir.stmts[stmt_id] {
                Stmt::Return(Some(e)) => hir.exprs[*e].byte_range(),
                Stmt::Throw(e) => hir.exprs[*e].byte_range(),
                _ => 0..0,
            }
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
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let res = resolve(&hir, &symbols);
        run_lints(&hir, &res, &symbols)
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

    // P19.10 follow-up
    /// `var _name = expr;` opts out of the
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

    /// Synthetic stdlib + user-source variant of `project_lints` for
    /// tests that exercise typed lints depending on annotations on
    /// `lib/std/core.gcl` decls (`@deref("resolve")` on node tags,
    /// `@iterable` on `Array<T>`, …). Without the stdlib in scope,
    /// those flags are missing from `ProjectIndex::type_flags` and
    /// the dependent lints fire spuriously.
    fn project_lints_with_stdlib(stdlib_src: &str, src: &str) -> Vec<LintDiagnostic> {
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let stdlib_uri = Uri::from_str("file:///std/core.gcl").unwrap();
        mgr.add_simple(stdlib_uri, stdlib_src, "std", false);
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        let pa = ProjectAnalysis::analyze(&mgr);
        pa.module(&uri).unwrap().lints.clone()
    }

    /// Minimal `lib/std/core.gcl` shape carrying `node<T>` +
    /// `@deref("resolve")` so the arrow-on-non-deref lint can see
    /// node tags as legitimately derefable.
    fn synthetic_std_core() -> &'static str {
        "native type any {}\n\
         native type bool {}\n\
         native type int {}\n\
         native type float {}\n\
         native type String {}\n\
         @deref(\"resolve\")\n\
         native type node<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeTime<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeList<T> {\n    fn resolve(): T;\n}\n\
         @deref(\"resolve\")\n\
         native type nodeGeo<T> {\n    fn resolve(): T;\n}\n\
         native type nodeIndex<K, V> {}\n\
         native type Array<T> { fn size(): int; }\n\
         native type Map<K, V> { fn size(): int; }\n"
    }

    #[test]
    fn arrow_on_node_tag_receiver_is_silent() {
        // `n->name` where `n: node<Foo>` is the canonical OK shape:
        // `node<T>` carries `@deref("resolve")` in stdlib so the
        // lint recognises it as derefable. Synthetic stdlib loaded
        // because the lint reads `TypeFlags::deref` and that table
        // only gets populated from real `.gcl` annotations.
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
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

    // -------------------------------------------------------------------
    // nullability hygiene (P19.17): possibly-null + redundant null-safe ops
    // -------------------------------------------------------------------

    #[test]
    fn possibly_null_on_nullable_member_access() {
        let diags = project_lints(
            r#"
fn f(x: String?) {
    var _ = x.size();
}
"#,
        );
        let hits: Vec<&LintDiagnostic> =
            diags.iter().filter(|d| d.rule == "possibly-null").collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one possibly-null hit, got {diags:?}"
        );
        assert!(
            hits[0].message.contains("`x`"),
            "expected ident in message: {}",
            hits[0].message
        );
    }

    #[test]
    fn upstream_null_safe_protects_downstream_chain() {
        // `x?.y.z` types as `int?` (chain propagation), but optional
        // chaining short-circuits at `?.y` — if `x` is null, `.z` is
        // never evaluated. So `possibly-null` must NOT fire on `.z`,
        // even though `x?.y`'s recorded type is nullable.
        let diags = project_lints(
            r#"
type Inner { z: int; }
type Outer { y: Inner; }
fn f(x: Outer?) {
    var _ = x?.y.z;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "downstream `.z` is protected by upstream `?.`; should not flag: {diags:?}"
        );
    }

    #[test]
    fn upstream_null_safe_through_call_protects_chain() {
        // `n?.resolve().chars()` — `?.` upstream of `.chars()` (going
        // through a call). The Call walks to its callee Member which
        // carries `pre_optional`; the chain helper must follow that
        // path to recognize the safe-suffix and suppress the lint.
        let diags = project_lints(
            r#"
fn f(n: node<String>?) {
    var _ = n?.resolve().chars();
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "`.chars()` after `n?.resolve()` is short-circuit-safe: {diags:?}"
        );
    }

    #[test]
    fn null_safe_access_suppresses_possibly_null() {
        let diags = project_lints(
            r#"
fn f(x: String?) {
    var _ = x?.size();
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "?. on nullable receiver should not flag possibly-null: {diags:?}"
        );
    }

    #[test]
    fn narrowed_receiver_suppresses_possibly_null() {
        // P19.16's narrows fold into `expr_types`, so the second `x`
        // visit is non-null. The lint reads the narrowed type — no
        // diagnostic should fire.
        let diags = project_lints(
            r#"
fn f(x: String?) {
    if (x != null) {
        var _ = x.size();
    }
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "narrowed receiver should not flag: {diags:?}"
        );
    }

    #[test]
    fn redundant_nullable_access_on_non_null_receiver() {
        let diags = project_lints(
            r#"
fn f(x: String) {
    var _ = x?.size();
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "redundant-nullable-access"),
            "expected redundant-nullable-access on non-null receiver: {diags:?}"
        );
    }

    #[test]
    fn redundant_non_null_assertion_on_non_null_operand() {
        let diags = project_lints(
            r#"
fn f(x: String) {
    var _ = x!!;
}
"#,
        );
        assert!(
            diags
                .iter()
                .any(|d| d.rule == "redundant-non-null-assertion"),
            "expected redundant-non-null-assertion on non-null operand: {diags:?}"
        );
    }

    #[test]
    fn redundant_non_null_assertion_silent_on_nullable() {
        let diags = project_lints(
            r#"
fn f(x: String?) {
    var _ = x!!;
}
"#,
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.rule == "redundant-non-null-assertion"),
            "!! on a nullable operand is the canonical use — should not flag: {diags:?}"
        );
    }

    #[test]
    fn redundant_coalesce_on_non_null_lhs() {
        let diags = project_lints(
            r#"
fn f(x: String) {
    var _ = x ?? "fallback";
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "redundant-coalesce"),
            "expected redundant-coalesce on non-null lhs: {diags:?}"
        );
    }

    #[test]
    fn coalesce_silent_on_nullable_lhs() {
        let diags = project_lints(
            r#"
fn f(x: String?) {
    var _ = x ?? "fallback";
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "redundant-coalesce"),
            "?? on nullable lhs is the canonical use — should not flag: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // P19.21 — narrowing extensions: `?=` and Arrow paths
    // -------------------------------------------------------------------

    #[test]
    fn coalesce_assign_narrows_member_lhs() {
        // `x.y ?= value` where `value` is non-null guarantees `x.y` is
        // non-null after the op. Subsequent reads must NOT flag
        // `possibly-null`. Mirrors the dominant pattern in the
        // solarleb corpus (`country->governorates ?= nodeIndex<…> {};`).
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
            r#"
type Bag { items: Map<String, int>?; }
fn f(b: Bag) {
    b.items ?= Map<String, int> {};
    var _ = b.items.size();
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "?= with non-null RHS should narrow LHS to non-null: {diags:?}"
        );
    }

    #[test]
    fn coalesce_assign_narrows_arrow_lhs() {
        // Same as above but with the `->` form on a node-tag receiver.
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
            r#"
type Inner { v: int; }
type Outer { items: Map<String, int>?; }
fn f(x: node<Outer>) {
    x->items ?= Map<String, int> {};
    var _ = x->items.size();
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "?= on arrow path should narrow: {diags:?}"
        );
    }

    #[test]
    fn arrow_path_if_null_guard_narrows_else_branch() {
        // `if (x->y == null) { ... } else { x->y.foo() }` — the else
        // branch should see `x->y` as non-null. Required Arrow support
        // in `member_compared_to_null` and `member_path`.
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
            r#"
type Inner { fn ping(): int { return 0; } }
type Outer { y: Inner?; }
fn f(x: node<Outer>) {
    if (x->y == null) {
        return;
    }
    var _ = x->y.ping();
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "post-guard arrow access should not flag: {diags:?}"
        );
    }

    #[test]
    fn arrow_path_short_circuit_and_narrows_rhs() {
        // `x->y != null && x->y.foo()` — the right operand of `&&`
        // sees the LHS narrows applied (P13.2-followup).
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
            r#"
type Inner { fn ping(): int { return 0; } }
type Outer { y: Inner?; }
fn f(x: node<Outer>) {
    var _ = x->y != null && x->y.ping() > 0;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "RHS of `&&` after `x->y != null` should not flag: {diags:?}"
        );
    }

    #[test]
    fn member_path_bang_bang_narrows_subsequent_reads() {
        // **P20.2** — `x.y!!` should narrow subsequent reads of the
        // same path (`x.y[0]`, `x.y.size()`) to non-null. Without the
        // narrow, every following access re-fires `possibly-null` on
        // the same field the user just asserted.
        let diags = project_lints(
            r#"
type Channel { name: String?; }
type Meta { channels: Array<Channel>?; }
fn f(meta: Meta) {
    var _ = meta.channels!!.size();
    var _ = meta.channels[0];
    var _ = meta.channels[1];
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "subsequent reads of a `!!`-asserted member path should not flag: {diags:?}"
        );
    }

    #[test]
    fn arrow_path_bang_bang_narrows_subsequent_reads() {
        // **P20.2** — same shape as the dot case but for `->`. Path
        // keys carry the operator, so dot-narrows and arrow-narrows
        // don't share state.
        let diags = project_lints_with_stdlib(
            synthetic_std_core(),
            r#"
type Inner { items: Array<int>?; }
type Outer { inner: Inner?; }
fn f(x: node<Outer>) {
    var _ = x->inner!!.items;
    var _ = x->inner.items;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "possibly-null"),
            "subsequent reads of an `x->y!!`-asserted arrow path should not flag: {diags:?}"
        );
    }

    #[test]
    fn member_path_bang_bang_narrow_drops_after_reassignment() {
        // **P20.2** — after `x.y!!` records the path as non-null,
        // a later `x.y = some_nullable` must drop the narrow so the
        // post-assign read sees the (re-introduced) nullable type.
        // This rides on the existing `record_assign_narrow` clear
        // path; the test pins the integration.
        let diags = project_lints(
            r#"
type Inner { name: String?; }
type Outer { y: Inner?; }
fn maybe(): Inner? { return null; }
fn f(x: Outer) {
    var _ = x.y!!.name;
    x.y = maybe();
    var _ = x.y.name;
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "possibly-null"),
            "post-reassignment read should re-flag possibly-null: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // P23 — directive-driven suppressions
    // -------------------------------------------------------------------

    #[test]
    fn lint_off_next_silences_unused_decl() {
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(
            uri.clone(),
            "// gcl-lint-next-off unused-decl\nprivate fn foo() {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let m = pa.module(&uri).unwrap();
        assert!(
            !m.lints.iter().any(|l| l.rule == "unused-decl"),
            "expected unused-decl to be silenced: {:?}",
            m.lints
        );
    }

    #[test]
    fn lint_off_next_unused_when_no_diagnostic_to_suppress() {
        // P23.3 — `gcl-lint-next-off unused-decl` on a non-private fn
        // (which `unused-decl` never fires on) is a dead toggle and
        // should surface `unused-suppression`.
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(
            uri.clone(),
            "// gcl-lint-next-off unused-decl\nfn callable() {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let m = pa.module(&uri).unwrap();
        assert!(
            m.lints.iter().any(|l| l.rule == "unused-suppression"),
            "expected unused-suppression on dead toggle: {:?}",
            m.lints
        );
    }

    #[test]
    fn unknown_rule_in_directive_surfaces_diagnostic() {
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(
            uri.clone(),
            "// gcl-lint-next-off not-a-rule\nfn foo() {}\n",
            "project",
            false,
        );
        let pa = ProjectAnalysis::analyze(&mgr);
        let m = pa.module(&uri).unwrap();
        assert!(
            m.lints.iter().any(|l| l.rule == "unknown-suppression-rule"),
            "expected unknown-suppression-rule: {:?}",
            m.lints
        );
    }

    // -------------------------------------------------------------------
    // `non-exhaustive` lint (promoted from a structural diagnostic; see
    // analyzer.rs `check_enum_exhaustiveness` + `lint_non_exhaustive_with_directives`)
    // -------------------------------------------------------------------

    #[test]
    fn non_exhaustive_lint_emits_for_uncovered_chain() {
        let diags = project_lints(
            r#"
enum Color { Red, Green, Blue }
fn pick(c: Color) {
    if (c == Color::Red) {
    } else if (c == Color::Green) {
    }
}
"#,
        );
        let hit = diags
            .iter()
            .find(|d| d.rule == "non-exhaustive")
            .expect("expected one `non-exhaustive` lint");
        assert!(hit.message.contains("Color"));
        assert!(hit.message.contains("Blue"));
    }

    #[test]
    fn non_exhaustive_lint_respects_lint_off_next() {
        // `// gcl-lint-next-off non-exhaustive` directly above the head
        // if drops the diagnostic. The directive must not also surface
        // an `unused-suppression` (it actually suppressed something).
        let diags = project_lints(
            r#"
enum Color { Red, Green, Blue }
fn pick(c: Color) {
    // gcl-lint-next-off non-exhaustive
    if (c == Color::Red) {
    } else if (c == Color::Green) {
    }
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "non-exhaustive"),
            "directive should suppress `non-exhaustive`, got {diags:?}"
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unused-suppression"),
            "active suppression must not be flagged unused, got {diags:?}"
        );
    }

    #[test]
    fn non_exhaustive_directive_on_exhaustive_chain_is_unused() {
        // The chain *is* exhaustive (every variant covered), so the
        // suppression has nothing to do — `unused-suppression` fires,
        // and no `non-exhaustive` lint is emitted in the first place.
        let diags = project_lints(
            r#"
enum Color { Red, Green, Blue }
fn pick(c: Color) {
    // gcl-lint-next-off non-exhaustive
    if (c == Color::Red) {
    } else if (c == Color::Green) {
    } else if (c == Color::Blue) {
    }
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "non-exhaustive"),
            "exhaustive chain should not flag, got {diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.rule == "unused-suppression" && d.message.contains("non-exhaustive")),
            "expected unused-suppression on dead `non-exhaustive` toggle: {diags:?}"
        );
    }

    // -------------------------------------------------------------------
    // P24.3 — `unreachable` lint
    // -------------------------------------------------------------------

    #[test]
    fn unreachable_after_return() {
        let diags = project_lints(
            r#"
fn f(): int {
    return 1;
    var _ = 0;
}
"#,
        );
        assert!(
            diags.iter().any(|d| d.rule == "unreachable"),
            "expected `unreachable` after return, got {diags:?}"
        );
    }

    #[test]
    fn unreachable_dead_else_on_exhaustive_chain() {
        // The user's example: `Some/None` covers `Option`; the
        // trailing `else { ... }` is dead.
        let diags = project_lints(
            r#"
enum Option { Some, None }
fn test(x: Option): int {
    if (x == Option::Some) {
        return 1;
    } else if (x == Option::None) {
        return -1;
    } else {
        return 0;
    }
}
"#,
        );
        let hits: Vec<&LintDiagnostic> = diags.iter().filter(|d| d.rule == "unreachable").collect();
        assert!(!hits.is_empty(), "expected dead else flagged: {diags:?}");
    }

    #[test]
    fn unreachable_post_chain_when_arms_diverge() {
        // The user's full example: dead else AND dead post-chain stmt.
        let diags = project_lints(
            r#"
enum Option { Some, None }
fn test(x: Option) {
    if (x == Option::Some) {
        return;
    } else if (x == Option::None) {
        return;
    } else {

    }
    var _ = 42;
}
"#,
        );
        let hits = diags.iter().filter(|d| d.rule == "unreachable").count();
        assert_eq!(
            hits, 2,
            "expected 2 unreachable diagnostics (dead else + post-chain), got {diags:?}"
        );
    }

    #[test]
    fn no_unreachable_when_chain_arms_dont_diverge() {
        // Chain is exhaustive but the variant arms don't return —
        // post-chain code is REACHABLE; only the trailing else (if any)
        // would be flagged. This tests "post-chain is not blanket-dead
        // just because the chain is exhaustive".
        let diags = project_lints(
            r#"
enum Option { Some, None }
fn test(x: Option): int {
    var r = 0;
    if (x == Option::Some) {
        r = 1;
    } else if (x == Option::None) {
        r = -1;
    }
    return r;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unreachable"),
            "post-chain code is reachable when arms don't diverge: {diags:?}"
        );
    }

    #[test]
    fn coalesces_contiguous_dead_siblings() {
        // Three dead stmts after a return → one coalesced diagnostic,
        // not three.
        let diags = project_lints(
            r#"
fn f(): int {
    return 1;
    var _ = 0;
    var _ = 1;
    var _ = 2;
}
"#,
        );
        let hits = diags.iter().filter(|d| d.rule == "unreachable").count();
        assert_eq!(hits, 1, "expected one coalesced diagnostic, got {diags:?}");
    }

    #[test]
    fn while_loop_does_not_make_post_loop_dead() {
        // Conservative on loops: even though the body returns, the
        // post-while statement is reachable (loop may not execute).
        let diags = project_lints(
            r#"
fn f(): int {
    while (true) {
        return 1;
    }
    return 0;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unreachable"),
            "post-while should be reachable: {diags:?}"
        );
    }

    #[test]
    fn outer_dead_island_dominates_nested_dead_code() {
        // P24.4: an outer dead island should swallow any inner dead
        // statements that sit inside it. Without dominance, we'd flag
        // both the outer post-return island AND the inner post-return
        // dead `var _ = 0;` separately.
        let diags = project_lints(
            r#"
fn f(): int {
    return 1;
    if (true) {
        return 2;
        var _ = 0;
    }
    var _ = 1;
}
"#,
        );
        let hits = diags.iter().filter(|d| d.rule == "unreachable").count();
        assert_eq!(
            hits, 1,
            "expected one outer dead island only (inner contained), got {diags:?}"
        );
    }

    #[test]
    fn dead_inside_reachable_branch_is_still_flagged() {
        // P24.4 sanity: when the OUTER block has no dead code but a
        // reachable inner block contains dead code, the inner dead
        // code IS flagged.
        let diags = project_lints(
            r#"
fn f(x: int): int {
    if (x > 0) {
        return 1;
        var _ = 0;
    }
    return 0;
}
"#,
        );
        let hits = diags.iter().filter(|d| d.rule == "unreachable").count();
        assert_eq!(hits, 1, "expected one inner dead diagnostic, got {diags:?}");
    }

    #[test]
    fn unreachable_suppressible_via_directive() {
        let diags = project_lints(
            r#"
fn f(): int {
    return 1;
    // gcl-lint-next-off unreachable
    var _ = 0;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| d.rule == "unreachable"),
            "directive should suppress unreachable: {diags:?}"
        );
    }

    #[test]
    fn nullability_lints_skip_any_receiver() {
        // `any` carries `nullable: true` in the arena, so a naive
        // `is_nullable` check would over-fire on every untyped value.
        // The lint family must skip `Any` and `Null` kinds explicitly.
        let diags = project_lints(
            r#"
fn pick(): any { return 1; }
fn f() {
    var x = pick();
    var _ = x.size();
    var _ = x ?? 0;
    var _ = x!!;
}
"#,
        );
        assert!(
            !diags.iter().any(|d| matches!(
                d.rule,
                "possibly-null" | "redundant-coalesce" | "redundant-non-null-assertion"
            )),
            "any-typed receivers/operands should not flag the nullability family: {diags:?}"
        );
    }

    #[test]
    fn redundant_semicolon_on_method_body() {
        // The headline case: stray `;` after a `type` method body. The
        // grammar permissively accepts it; the lint must flag the single
        // `;` byte range (not the surrounding span — that's the whole
        // point of pulling the recovery in via `block_trailing_semi`).
        let src = "type T {\n    static fn m(): int { return 0; };\n}\n";
        let diags = project_lints(src);
        let hit = diags
            .iter()
            .find(|d| d.rule == "redundant-semicolon")
            .expect("expected one `redundant-semicolon` lint");
        assert_eq!(hit.severity, LintSeverity::Error);
        assert_eq!(
            &src[hit.byte_range.clone()],
            ";",
            "diagnostic should pin to exactly the stray `;`"
        );
    }

    #[test]
    fn redundant_semicolon_on_top_level_fn_body() {
        // Same shape but on a top-level `fn_decl`; lints the `;`
        // after the body's closing `}`.
        let src = "fn n(): int { return 1; };\n";
        let diags = project_lints(src);
        let hit = diags
            .iter()
            .find(|d| d.rule == "redundant-semicolon")
            .expect("expected one `redundant-semicolon` lint");
        assert_eq!(&src[hit.byte_range.clone()], ";");
    }

    #[test]
    fn redundant_semicolon_silent_on_canonical_form() {
        // No trailing `;` after the body — no diagnostic. Anchors the
        // permissive-grammar change against accidentally flagging
        // every method.
        let diags = project_lints("type T {\n    static fn m(): int { return 0; }\n}\n");
        assert!(!diags.iter().any(|d| d.rule == "redundant-semicolon"));
    }

    #[test]
    fn redundant_semicolon_silent_on_native_method() {
        // Native (no-body) methods are still allowed to end with `;`
        // — that's the `_semi` alternative in the grammar, not
        // `block_trailing_semi`. The lint must not fire.
        let diags = project_lints("type T {\n    fn m();\n}\n");
        assert!(!diags.iter().any(|d| d.rule == "redundant-semicolon"));
    }

    #[test]
    fn redundant_semicolon_covers_multi_semi_run() {
        // `};;;` parses as one `block_trailing_semi` whose range
        // spans all three `;`s. The lint's message switches to plural
        // ("drop them") and the quickfix removes the whole run.
        let src = "fn n() { var x = 0; };;;\n";
        let diags = project_lints(src);
        let hit = diags
            .iter()
            .find(|d| d.rule == "redundant-semicolon")
            .expect("expected one `redundant-semicolon` lint");
        assert_eq!(&src[hit.byte_range.clone()], ";;;");
        assert!(hit.message.contains("drop them"), "got: {}", hit.message);
    }

    #[test]
    fn redundant_semicolon_respects_lint_off_next() {
        // `// gcl-lint-next-off redundant-semicolon` directly above
        // the offending fn drops the diagnostic — proves the rule
        // plugs into the standard suppression machinery.
        let diags =
            project_lints("// gcl-lint-next-off redundant-semicolon\nfn n(): int { return 1; };\n");
        assert!(!diags.iter().any(|d| d.rule == "redundant-semicolon"));
    }
}
