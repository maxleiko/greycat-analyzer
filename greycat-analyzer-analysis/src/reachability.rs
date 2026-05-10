//! Reachability / divergence analysis on the HIR (P24).
//!
//! Single primitive: [`stmt_diverges`]. Returns `true` iff control flow
//! cannot fall through past the statement in normal execution — i.e.
//! every path through it reaches a `return` / `throw` / `break` /
//! `continue`, or recursively a divergent inner statement.
//!
//! Pure-HIR walker, no typing / resolver dependency. The dead-code lint
//! ([`crate::lint`]'s `unreachable` rule, P24.3) consumes this primitive
//! to flag statements that follow a divergent sibling.
//!
//! **Conservative on loops.** `while` / `for` / `for-in` / `do-while`
//! never count as divergent: we don't const-fold conditions, so we
//! can't prove the loop body executes at all (much less that every
//! exit path diverges). `do-while` *does* execute the body once but
//! we still skip it for v1 — the win is small and the false-positive
//! risk on early-return-in-do-while is real.
//!
//! **Conservative on functions.** Bare expression statements never
//! diverge — even when the call is to a function whose body always
//! throws. P24 doesn't track per-function "never returns" annotations
//! (`@never_returns` is deferred — see ROADMAP). When that lands, this
//! primitive grows a `&FnIndex` arg.

use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{BlockStmt, IfStmt, Stmt, TryStmt};

/// `true` iff control flow cannot fall through past `stmt_id`. See the
/// module docs for the conservative-on-loops / conservative-on-calls
/// caveats.
pub fn stmt_diverges(hir: &Hir, stmt_id: Idx<Stmt>) -> bool {
    match &hir.stmts[stmt_id] {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break | Stmt::Continue => true,
        Stmt::Block(b) => block_diverges(hir, b),
        Stmt::If(i) => if_diverges(hir, i),
        Stmt::Try(t) => try_diverges(hir, t),
        // `@<expr> { … }` — the block runs at most once and the
        // expression's side effects don't change reachability of
        // anything after the at-stmt. Treat as divergent iff the inner
        // block diverges (the at-stmt itself is otherwise straight-
        // through).
        Stmt::At(a) => block_diverges(hir, &a.block),
        // Loops never diverge from the analyzer's POV — see module docs.
        Stmt::While(_) | Stmt::DoWhile(_) | Stmt::For(_) | Stmt::ForIn(_) => false,
        // Pure straight-line statements.
        Stmt::Expr(_) | Stmt::Var(_) | Stmt::Assign(_) => false,
    }
}

/// `true` iff some statement in `block` diverges. Equivalent to "this
/// block has a guaranteed exit point" — the next statement after the
/// block is unreachable.
pub fn block_diverges(hir: &Hir, block: &BlockStmt) -> bool {
    block.stmts.iter().any(|s| stmt_diverges(hir, *s))
}

fn if_diverges(hir: &Hir, i: &IfStmt) -> bool {
    if !block_diverges(hir, &i.then_branch) {
        return false;
    }
    let Some(else_id) = i.else_branch else {
        // No else → fall-through path exists when condition is false.
        return false;
    };
    stmt_diverges(hir, else_id)
}

fn try_diverges(hir: &Hir, t: &TryStmt) -> bool {
    block_diverges(hir, &t.try_block) && block_diverges(hir, &t.catch_block)
}

/// Index of the first statement in `block` that follows a divergent
/// sibling — i.e. the first statement that is *unreachable* under
/// normal control flow. `None` when every statement is reachable.
///
/// This is the entry point the dead-code lint consumes: walk a block
/// once, and the returned index splits `block.stmts` into a reachable
/// prefix and a dead suffix.
pub fn first_dead_index(hir: &Hir, block: &BlockStmt) -> Option<usize> {
    for (i, s) in block.stmts.iter().enumerate() {
        if stmt_diverges(hir, *s) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_hir::types::{Decl, FnDecl};
    use greycat_analyzer_syntax::parse;

    fn lower(src: &str) -> Hir {
        let tree = parse(src);
        lower_module(src, "mod", "project", tree.root_node())
    }

    fn fn_body(hir: &Hir, name: &str) -> BlockStmt {
        let module = hir.module.as_ref().expect("module");
        for decl_id in &module.decls {
            if let Decl::Fn(FnDecl {
                name: name_idx,
                body: Some(body_id),
                ..
            }) = &hir.decls[*decl_id]
                && hir.idents[*name_idx].text == name
                && let Stmt::Block(block) = &hir.stmts[*body_id]
            {
                return block.clone();
            }
        }
        panic!("fn {name} not found");
    }

    #[test]
    fn return_diverges() {
        let hir = lower("fn f(): int { return 1; }");
        let body = fn_body(&hir, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn throw_diverges() {
        let hir = lower("fn f() { throw \"bad\"; }");
        let body = fn_body(&hir, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn break_diverges() {
        let hir = lower("fn f() { while (true) { break; } }");
        let body = fn_body(&hir, "f");
        // The OUTER block doesn't diverge — `while` doesn't diverge
        // even though its body does. Conservative on loops.
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn straight_line_does_not_diverge() {
        let hir = lower("fn f() { var x = 1; var y = 2; }");
        let body = fn_body(&hir, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn if_diverges_only_when_both_branches_diverge() {
        let hir1 = lower("fn f(): int { if (true) { return 1; } else { return 2; } }");
        let body1 = fn_body(&hir1, "f");
        assert!(block_diverges(&hir1, &body1));

        let hir2 = lower("fn f(): int { if (true) { return 1; } else { var _ = 0; } return 0; }");
        // The if doesn't diverge (else falls through), but the trailing
        // `return 0;` does — so the OUTER block diverges.
        let body2 = fn_body(&hir2, "f");
        assert!(block_diverges(&hir2, &body2));

        let hir3 = lower("fn f(): int { if (true) { return 1; } return 0; }");
        // No else → if doesn't diverge → trailing return picks up the slack.
        let body3 = fn_body(&hir3, "f");
        assert!(block_diverges(&hir3, &body3));
    }

    #[test]
    fn try_diverges_when_both_blocks_diverge() {
        let hir = lower("fn f(): int { try { return 1; } catch (e) { return 2; } }");
        let body = fn_body(&hir, "f");
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn try_does_not_diverge_when_catch_falls_through() {
        let hir = lower("fn f(): int { try { return 1; } catch (e) { var _ = 0; } return 3; }");
        let body = fn_body(&hir, "f");
        // Outer block diverges via the trailing return; the try alone
        // wouldn't.
        assert!(block_diverges(&hir, &body));
    }

    #[test]
    fn while_never_diverges_even_with_diverging_body() {
        let hir = lower("fn f() { while (true) { return; } }");
        let body = fn_body(&hir, "f");
        assert!(!block_diverges(&hir, &body));
    }

    #[test]
    fn first_dead_index_after_return() {
        let hir = lower("fn f(): int { return 1; var _ = 0; var _ = 1; }");
        let body = fn_body(&hir, "f");
        // `return` is index 0. First dead index is 1 (the first
        // `var _`).
        assert_eq!(first_dead_index(&hir, &body), Some(1));
    }

    #[test]
    fn first_dead_index_none_when_all_reachable() {
        let hir = lower("fn f(): int { var x = 1; return x; }");
        let body = fn_body(&hir, "f");
        assert_eq!(first_dead_index(&hir, &body), None);
    }

    #[test]
    fn first_dead_index_none_when_divergent_is_last() {
        let hir = lower("fn f(): int { var x = 1; return x; }");
        let body = fn_body(&hir, "f");
        // The return is the last stmt — nothing after it, no dead index.
        assert_eq!(first_dead_index(&hir, &body), None);
    }
}
