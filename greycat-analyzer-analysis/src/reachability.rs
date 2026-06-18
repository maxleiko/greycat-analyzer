//! Reachability / divergence analysis on the HIR.
//!
//! Single primitive: [`stmt_diverges`]. Returns `true` iff control flow
//! cannot fall through past the statement in normal execution — i.e.
//! every path through it reaches a `return` / `throw` / `break` /
//! `continue`, or recursively a divergent inner statement.
//!
//! A structural HIR walker that folds in two `AnalysisResult` facts
//! (`decidable_conditions`, `exhaustive_enum_chains`); pass an empty
//! `AnalysisResult` for the purely-structural view. The dead-code lint
//! ([`crate::lint`]'s `unreachable` rule) and the `missing-return`
//! check both consume this primitive.
//!
//! **Conservative on loops, with one exception.** Loops with a
//! non-literal condition stay non-divergent: we don't const-fold, so we
//! can't prove the body runs or that every exit diverges. The lone
//! exception is the unconditional infinite loop — `while (true)` and a
//! `for (var i = ...; true; ...)` with a literal-`true` condition — which
//! diverges when no `break` targets it. `do-while` / `for-in` stay
//! conservative.
//!
//! **Conservative on functions.** Bare expression statements never
//! diverge — even when the call is to a function whose body always
//! throws.

use std::ops::Range;

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::hir::{AtStmt, BlockStmt, Expr, IfStmt, LiteralKind, Stmt, TryStmt};

use crate::analyzer::AnalysisResult;

/// `true` iff control flow cannot fall through past `stmt_id`. The HIR
/// structural walker (loops, try/catch, break-targeting) plus two facts
/// it can't see on its own, folded in recursively at every nesting
/// depth. An `if` whose condition the analyzer proved statically
/// `true` / `false` (`decidable_conditions`) collapses to its one live
/// branch — so `if (s is Poly) { return ...; }` where `s` is already
/// narrowed to `Poly` diverges despite having no `else`. And an
/// exhaustive enum-eq if-chain (`exhaustive_enum_chains`) diverges when
/// every arm body diverges, even without a trailing `else`. Pass an
/// empty `AnalysisResult` for the purely-structural view.
pub fn stmt_diverges(hir: &Hir, analysis: &AnalysisResult, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break(_) | Stmt::Continue(_) => true,
        Stmt::Block(b) => block_diverges_impl(hir, analysis, b),
        Stmt::If(i) => if_diverges_impl(hir, analysis, stmt_id, i),
        Stmt::Try(t) => {
            block_diverges_impl(hir, analysis, &t.try_block)
                && block_diverges_impl(hir, analysis, &t.catch_block)
        }
        // `@<expr> { … }` — the block runs at most once and the
        // expression's side effects don't change reachability of
        // anything after the at-stmt. Treat as divergent iff the inner
        // block diverges (the at-stmt itself is otherwise straight-
        // through).
        Stmt::At(a) => block_diverges_impl(hir, analysis, &a.block),
        // Narrow infinite-loop rule: a `while (true)` or a C-style
        // `for (var i = ...; true; ...)` with a literal-`true` condition
        // and no `break` targeting it can never fall through. Any other
        // loop stays conservative (`false`) — we don't const-fold
        // non-literal conditions, and a break makes the exit reachable.
        // (GreyCat's `for` requires all three clauses; there is no
        // `for (;;)`.)
        Stmt::While(w) => {
            is_true_literal(hir, w.condition) && !block_breaks_current_loop(hir, &w.body)
        }
        Stmt::For(f) => {
            f.condition.is_some_and(|c| is_true_literal(hir, c))
                && !block_breaks_current_loop(hir, &f.body)
        }
        Stmt::DoWhile(_) | Stmt::ForIn(_) => false,
        // Pure straight-line statements. `breakpoint` is intentionally
        // here because it pauses the worker for debugging, then execution
        // resumes from the next statement. Treating it as divergent
        // would mark legitimate post-debug code as unreachable.
        Stmt::Expr(_) | Stmt::Var(_) | Stmt::Assign(_) | Stmt::Breakpoint(_) => false,
    }
}

/// `true` iff some statement in `block` diverges. Equivalent to "this
/// block has a guaranteed exit point" — the next statement after the
/// block is unreachable. HIR-only convenience used by the unit tests.
#[cfg(test)]
fn block_diverges(hir: &Hir, block: &BlockStmt) -> bool {
    block_diverges_impl(hir, &AnalysisResult::default(), block)
}

fn block_diverges_impl(hir: &Hir, analysis: &AnalysisResult, block: &BlockStmt) -> bool {
    block.stmts.iter().any(|s| stmt_diverges(hir, analysis, *s))
}

fn if_diverges_impl(hir: &Hir, analysis: &AnalysisResult, stmt_id: Idx<Stmt>, i: &IfStmt) -> bool {
    // A statically-decidable condition collapses the `if` to its one
    // live path: always-true → only the then-branch runs; always-false
    // → only the else (or fall-through) runs.
    match analysis.decidable_conditions.get(&stmt_id) {
        Some(true) => return block_diverges_impl(hir, analysis, &i.then_branch),
        Some(false) => {
            return i
                .else_branch
                .is_some_and(|eb| stmt_diverges(hir, analysis, eb));
        }
        None => {}
    }
    // Exhaustive enum-eq chain with no else: every value is covered, so
    // the chain diverges when every arm body diverges.
    if analysis.exhaustive_enum_chains.contains(&stmt_id)
        && every_arm_diverges(hir, analysis, stmt_id)
    {
        return true;
    }
    if !block_diverges_impl(hir, analysis, &i.then_branch) {
        return false;
    }
    let Some(else_id) = i.else_branch else {
        // No else → fall-through path exists when condition is false.
        return false;
    };
    stmt_diverges(hir, analysis, else_id)
}

/// `true` iff `expr_id` is the literal `true`.
fn is_true_literal(hir: &Hir, expr_id: Idx<Expr>) -> bool {
    matches!(
        &hir.exprs[expr_id],
        Expr::Literal(l) if l.kind == LiteralKind::Bool(true)
    )
}

/// `true` if `block` contains a `Stmt::Break` whose target is the
/// enclosing loop — i.e. not enclosed in a nested loop. GreyCat's
/// `break` carries no labels (`Stmt::Break` is a unit variant per
/// `greycat-analyzer-hir`), so a break inside a nested `while` / `for` /
/// `do-while` / `for-in` unambiguously targets that inner loop; we stop
/// walking at those. Used by the infinite-loop divergence rule above and
/// by the post-loop else-narrow lift in the analyzer.
pub(crate) fn block_breaks_current_loop(hir: &Hir, block: &BlockStmt) -> bool {
    block
        .stmts
        .iter()
        .any(|s| stmt_breaks_current_loop(hir, *s))
}

fn stmt_breaks_current_loop(hir: &Hir, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Break(_) => true,
        Stmt::While(_) | Stmt::For(_) | Stmt::DoWhile(_) | Stmt::ForIn(_) => false,
        Stmt::Block(b) => block_breaks_current_loop(hir, b),
        Stmt::If(IfStmt {
            then_branch,
            else_branch,
            ..
        }) => {
            block_breaks_current_loop(hir, then_branch)
                || else_branch.is_some_and(|eb| stmt_breaks_current_loop(hir, eb))
        }
        Stmt::Try(TryStmt {
            try_block,
            catch_block,
            ..
        }) => {
            block_breaks_current_loop(hir, try_block) || block_breaks_current_loop(hir, catch_block)
        }
        Stmt::At(AtStmt { block, .. }) => block_breaks_current_loop(hir, block),
        Stmt::Expr(_)
        | Stmt::Var(_)
        | Stmt::Assign(_)
        | Stmt::Return(_)
        | Stmt::Continue(_)
        | Stmt::Breakpoint(_)
        | Stmt::Throw(_) => false,
    }
}

/// `true` iff every reachable arm body in the if-chain rooted at
/// `head_id` diverges. Walks the chain via the `else_branch` field
/// (same shape [`crate::analyzer::Cx::extract_enum_chain`] follows),
/// checking each variant-matching `then_branch` for divergence. The
/// trailing `else { … }` arm is *ignored* — under exhaustive
/// coverage it's already dead (`dead_else_range_for_exhaustive_chain`
/// flags it independently), and requiring its (often empty) body to
/// diverge would suppress the post-chain dead-code signal in the
/// canonical "Some/None arms both return; else { } is dead;
/// post-chain code is dead" shape.
fn every_arm_diverges(hir: &Hir, analysis: &AnalysisResult, head_id: Idx<Stmt>) -> bool {
    let mut cur = head_id;
    loop {
        match &hir.stmts[cur] {
            Stmt::If(IfStmt {
                then_branch,
                else_branch,
                ..
            }) => {
                if !block_diverges_impl(hir, analysis, then_branch) {
                    return false;
                }
                let Some(eb) = else_branch else {
                    // No else — chain has no fall-through arm, so
                    // "every arm" is just every then-branch. We've
                    // walked them all.
                    return true;
                };
                cur = *eb;
            }
            Stmt::Block(_) => {
                // Final `else { … }` arm — under exhaustive coverage
                // (the only context that calls this helper), the else
                // is dead anyway. Treat as "doesn't block divergence".
                return true;
            }
            _ => {
                // Anything else in the else-branch slot (e.g. a single
                // statement) — fall back to the divergence primitive.
                return stmt_diverges(hir, analysis, cur);
            }
        }
    }
}

// P24.2
/// Byte range of the trailing `else { … }` block of an
/// exhaustive chain. Returns `None` when the chain has no final else
/// (no dead arm to flag) or the head doesn't match the chain shape.
/// The dead-code lint emits `unreachable` at this range.
pub fn dead_else_range_for_exhaustive_chain(hir: &Hir, head_id: Idx<Stmt>) -> Option<Range<usize>> {
    let mut cur = head_id;
    loop {
        match &hir.stmts[cur] {
            Stmt::If(IfStmt { else_branch, .. }) => {
                let Some(eb) = else_branch else {
                    return None;
                };
                cur = *eb;
            }
            Stmt::Block(b) => return Some(b.byte_range.clone()),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_hir::hir::{Decl, FnDecl};
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;

    fn lower(src: &str) -> (Hir, SymbolTable) {
        let tree = parse(src);
        let symbols = SymbolTable::new();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        (hir, symbols)
    }

    fn fn_body(hir: &Hir, symbols: &SymbolTable, name: &str) -> BlockStmt {
        let module = hir.module.as_ref().expect("module");
        let needle = symbols.lookup(name).expect("name interned");
        for decl_id in &module.decls {
            if let Decl::Fn(FnDecl {
                name: name_idx,
                body: Some(body_id),
                ..
            }) = &hir.decls[*decl_id]
                && hir.idents[*name_idx].symbol == needle
                && let Stmt::Block(block) = &hir.stmts[*body_id]
            {
                return block.clone();
            }
        }
        panic!("fn {name} not found");
    }

    /// Index of the first statement in `block` that follows a divergent
    /// sibling — i.e. the first statement that is *unreachable* under
    /// normal control flow. `None` when every statement is reachable.
    fn first_dead_index(hir: &Hir, block: &BlockStmt) -> Option<usize> {
        let analysis = AnalysisResult::default();
        for (i, s) in block.stmts.iter().enumerate() {
            if stmt_diverges(hir, &analysis, *s) {
                // Everything after `i` is unreachable. The divergent
                // statement itself is reachable — `i + 1` is the first
                // dead index.
                return if i + 1 < block.stmts.len() {
                    Some(i + 1)
                } else {
                    None
                };
            }
        }
        None
    }

    #[test]
    fn return_diverges() {
        let (hir, symbols) = lower("fn f(): int { return 1; }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn throw_diverges() {
        let (hir, symbols) = lower("fn f() { throw \"bad\"; }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn break_diverges() {
        let (hir, symbols) = lower("fn f() { while (true) { break; } }");
        let body = fn_body(&hir, &symbols, "f");
        // The OUTER block doesn't diverge — `while` doesn't diverge
        // even though its body does. Conservative on loops.
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn straight_line_does_not_diverge() {
        let (hir, symbols) = lower("fn f() { var x = 1; var y = 2; }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn if_diverges_only_when_both_branches_diverge() {
        let (hir1, symbols1) = lower("fn f(): int { if (true) { return 1; } else { return 2; } }");
        let body1 = fn_body(&hir1, &symbols1, "f");
        assert!(block_diverges(&hir1, &body1));

        let (hir2, symbols2) =
            lower("fn f(): int { if (true) { return 1; } else { var _ = 0; } return 0; }");
        // The if doesn't diverge (else falls through), but the trailing
        // `return 0;` does — so the OUTER block diverges.
        let body2 = fn_body(&hir2, &symbols2, "f");
        assert!(block_diverges(&hir2, &body2));

        let (hir3, symbols3) = lower("fn f(): int { if (true) { return 1; } return 0; }");
        // No else → if doesn't diverge → trailing return picks up the slack.
        let body3 = fn_body(&hir3, &symbols3, "f");
        assert!(block_diverges(&hir3, &body3));
    }

    #[test]
    fn try_diverges_when_both_blocks_diverge() {
        let (hir, symbols) = lower("fn f(): int { try { return 1; } catch (e) { return 2; } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn try_does_not_diverge_when_catch_falls_through() {
        let (hir, symbols) =
            lower("fn f(): int { try { return 1; } catch (e) { var _ = 0; } return 3; }");
        let body = fn_body(&hir, &symbols, "f");
        // Outer block diverges via the trailing return; the try alone
        // wouldn't.
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn while_true_without_break_diverges() {
        let (hir, symbols) = lower("fn f() { while (true) { var _ = 0; } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn while_true_with_break_does_not_diverge() {
        let (hir, symbols) = lower("fn f() { while (true) { if (g()) { break; } } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn while_true_with_break_in_nested_loop_still_diverges() {
        // The inner `break` targets the inner loop, not the `while (true)`.
        let (hir, symbols) =
            lower("fn f() { while (true) { while (g()) { if (g()) { break; } } } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn while_non_literal_condition_does_not_diverge() {
        let (hir, symbols) = lower("fn f() { while (g()) { var _ = 0; } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn for_with_literal_true_condition_without_break_diverges() {
        let (hir, symbols) = lower("fn f() { for (var i = 0; true; i = i + 1) { var _ = 0; } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn for_with_non_literal_condition_does_not_diverge() {
        let (hir, symbols) = lower("fn f() { for (var i = 0; i < 10; i = i + 1) { var _ = 0; } }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn do_while_true_stays_conservative() {
        let (hir, symbols) = lower("fn f() { do { var _ = 0; } while (true); }");
        let body = fn_body(&hir, &symbols, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn first_dead_index_after_return() {
        let (hir, symbols) = lower("fn f(): int { return 1; var _ = 0; var _ = 1; }");
        let body = fn_body(&hir, &symbols, "f");
        // `return` is index 0. First dead index is 1 (the first
        // `var _`).
        assert_eq!(first_dead_index(&hir, &body), Some(1));
    }

    #[test]
    fn first_dead_index_none_when_all_reachable() {
        let (hir, symbols) = lower("fn f(): int { var x = 1; return x; }");
        let body = fn_body(&hir, &symbols, "f");
        assert_eq!(first_dead_index(&hir, &body), None);
    }

    #[test]
    fn first_dead_index_none_when_divergent_is_last() {
        let (hir, symbols) = lower("fn f(): int { var x = 1; return x; }");
        let body = fn_body(&hir, &symbols, "f");
        // The return is the last stmt — nothing after it, no dead index.
        assert_eq!(first_dead_index(&hir, &body), None);
    }

    // -------------------------------------------------------------------
    // P24.2 — exhaustive-chain divergence promotion
    // -------------------------------------------------------------------

    fn project_analyze(
        src: &str,
    ) -> (
        greycat_analyzer_core::lsp_types::Uri,
        crate::project::ProjectAnalysis,
    ) {
        use crate::project::ProjectAnalysis;
        use greycat_analyzer_core::SourceManager;
        use greycat_analyzer_core::lsp_types::Uri;
        use std::str::FromStr;
        let mut mgr = SourceManager::new();
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        mgr.add_simple(uri.clone(), src, "project", false);
        (uri, ProjectAnalysis::analyze(&mgr))
    }

    #[test]
    fn exhaustive_chain_recorded_when_all_variants_covered() {
        let (uri, pa) = project_analyze(
            "enum E { A, B }\nfn f(x: E): int { if (x == E::A) { return 1; } else if (x == E::B) { return 2; } return 0; }\n",
        );
        let m = pa.module(&uri).unwrap();
        assert_eq!(
            m.analysis.exhaustive_enum_chains.len(),
            1,
            "expected one exhaustive chain"
        );
    }

    #[test]
    fn non_exhaustive_chain_not_recorded() {
        let (uri, pa) = project_analyze(
            "enum E { A, B, C }\nfn f(x: E): int { if (x == E::A) { return 1; } else if (x == E::B) { return 2; } return 0; }\n",
        );
        let m = pa.module(&uri).unwrap();
        assert!(
            m.analysis.exhaustive_enum_chains.is_empty(),
            "non-exhaustive chains should not be recorded"
        );
    }

    #[test]
    fn analysis_promotes_exhaustive_chain() {
        let (uri, pa) = project_analyze(
            "enum E { A, B }\nfn f(x: E): int { if (x == E::A) { return 1; } else if (x == E::B) { return 2; } return 0; }\n",
        );
        let m = pa.module(&uri).unwrap();
        let body = fn_body(&m.hir, pa.symbols(), "f");
        // The exhaustive chain is at index 0; every arm returns; so with
        // the analysis facts it should be divergent.
        let head_id = body.stmts[0];
        assert!(stmt_diverges(&m.hir, &m.analysis, head_id));
        // With an empty analysis the walker can't know about enum
        // exhaustiveness (no else arm), so it returns false.
        assert!(!stmt_diverges(&m.hir, &AnalysisResult::default(), head_id));
    }

    #[test]
    fn dead_else_range_for_exhaustive_chain_returns_else_block_span() {
        let src = "enum E { A, B }\nfn f(x: E): int { if (x == E::A) { return 1; } else if (x == E::B) { return 2; } else { return 3; } }\n";
        let (uri, pa) = project_analyze(src);
        let m = pa.module(&uri).unwrap();
        let body = fn_body(&m.hir, pa.symbols(), "f");
        let head_id = body.stmts[0];
        assert!(m.analysis.exhaustive_enum_chains.contains(&head_id));
        let dead_range =
            dead_else_range_for_exhaustive_chain(&m.hir, head_id).expect("should find else block");
        let covered = &src[dead_range];
        assert!(
            covered.contains("return 3"),
            "expected else block contents in dead range, got {covered:?}"
        );
    }
}
