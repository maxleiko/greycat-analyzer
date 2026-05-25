//! Body-driven return-type inference.
//!
//! Walks a function / lambda body's `return` statements and computes
//! the single GCL-expressible type that covers every reachable
//! `return e;`. Returns `None` when:
//! - no `return` statement was seen (caller defaults appropriately),
//! - branches disagree and the join doesn't reduce to `T` or `T?`,
//! - the resulting type isn't writable in a GCL `type_ident` slot
//!   (filter via [`is_expressible_type_ident`]).
//!
//! Consumed by two callers:
//! - [`crate::lint`]'s `infer-return-type` rule, which emits a hint
//!   when a fn-decl has no declared return type but the body does
//!   produce a single inferrable shape.
//! - The analyzer's `Expr::Lambda` typing arm, which uses the same
//!   inference to populate `TypeKind::Lambda.ret: Option<TypeId>` so
//!   `fn() { return 5; }` displays as `fn(): int` and calls through
//!   the lambda check arg-types against the right shape.
//!
//! The "lambda is a top-level fn in a scope" mental model is enforced
//! by sharing this implementation — there's no second copy of these
//! rules.

use greycat_analyzer_core::{Type, TypeArena, TypeId, TypeKind};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::Stmt;

use crate::analyzer::AnalysisResult;

/// Walk the fn / lambda body recursively, collect the settled type of
/// every `return e;`, and return the joined single type — or `None`
/// when the branches form a union the GCL surface can't write.
///
/// Agreement is by exact `TypeId` when the analyzer happens to record
/// the same id for both branches. The interesting case is when ids
/// differ but a single expressible type still covers both:
///
/// - `null + T?` (or `T? + null`) collapses to `T?`. The right-hand
///   branch's type already carries the nullable bit, so the join is
///   the non-null-kind branch's id. No arena allocation needed.
/// - `T + null` (where `T` is non-nullable) collapses to `T?`. The
///   nullable wrap is found via the arena's intern table — read-only,
///   no allocation. If the wrap doesn't already exist in the arena
///   (no other code in the project used `T?`) we conservatively bail.
/// - `T + T?` (same kind, one nullable) collapses to `T?` via the
///   same intern lookup.
///
/// Other mixed shapes (`float? + bool`, `int + String`, ...) form a
/// union with no GCL syntax and bail. The caller's
/// [`is_expressible_type_ident`] gate is a second line of defense
/// against the same shape leaking through (e.g. via a `Lambda` /
/// `TypeOf` return) when the branches happen to share an id.
pub fn inferred_return_from_body(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    body: Idx<Stmt>,
) -> Option<TypeId> {
    let mut seen: Option<TypeId> = None;
    if collect_return_types(hir, analysis, arena, body, &mut seen).is_err() {
        return None;
    }
    seen
}

/// Sibling of [`inferred_return_from_body`] for callers that hold a
/// `BlockStmt` directly (e.g. the analyzer's `Expr::Lambda` arm —
/// `LambdaExpr.body` is a `BlockStmt`, not a wrapped `Stmt::Block`).
/// Same rules, same return semantics; just enters the recursion via
/// `collect_returns_in_block` directly so the caller doesn't need a
/// temporary `Stmt::Block` allocation.
pub fn inferred_return_from_block(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    body: &greycat_analyzer_hir::types::BlockStmt,
) -> Option<TypeId> {
    let mut seen: Option<TypeId> = None;
    if collect_returns_in_block(hir, analysis, arena, &body.stmts, &mut seen).is_err() {
        return None;
    }
    seen
}

/// Combine `a` and `b` into the single GCL-expressible type that
/// covers both, or `None` when no such type exists in the arena. See
/// [`inferred_return_from_body`] for the rule set.
pub fn join_return_types(arena: &TypeArena, a: TypeId, b: TypeId) -> Option<TypeId> {
    if a == b {
        return Some(a);
    }
    let ta = arena.get(a);
    let tb = arena.get(b);
    let nullable_companion = |id: TypeId| -> Option<TypeId> {
        let t = arena.get(id);
        if t.nullable {
            return Some(id);
        }
        let probe = Type {
            kind: t.kind.clone(),
            nullable: true,
        };
        arena.intern.get(&probe).copied()
    };
    // `null + T` (either order) → the nullable form of T. Includes the
    // `T?` case (the companion of an already-nullable type is itself).
    if matches!(ta.kind, TypeKind::Null) {
        return nullable_companion(b);
    }
    if matches!(tb.kind, TypeKind::Null) {
        return nullable_companion(a);
    }
    // Same kind, differ only in `nullable` → `T?`. Looks the nullable
    // wrap up read-only.
    if ta.kind == tb.kind && ta.nullable != tb.nullable {
        return nullable_companion(if ta.nullable { a } else { b });
    }
    None
}

/// Recursive helper for [`inferred_return_from_body`]. Visits every
/// statement reachable from `stmt_id` and folds each `Stmt::Return`'s
/// value type into `seen`. Returns `Err(())` the first time it sees a
/// return whose type differs from `seen` — the caller treats that as
/// "branches disagree, no expressible single type, skip the hint".
///
/// Visits the bodies of `Block` / `If` / `While` / `DoWhile` / `For` /
/// `ForIn` / `Try` / `At` so a return buried inside an if-then or
/// loop counts the same as one in the outer block. `Stmt::Return(None)`
/// has no value type — leaves `seen` untouched (a bare `return;` in a
/// branch alongside `return 42;` shouldn't change the inference; if
/// the user wanted a hint they'd write the explicit return).
///
/// Dead branches are skipped: at each block we stop iterating after
/// the first statement that `stmt_diverges_with_analysis` proves
/// terminates control flow. That mirrors the `unreachable` lint's
/// reasoning so a return in a provably-dead branch (e.g. after an
/// earlier `return` / `throw` in the same block) doesn't pollute the
/// inferred type. Narrow-dead branches (a then-arm whose condition
/// the analyzer proves is statically false) aren't covered — the
/// reachability primitive doesn't track condition-falseness today;
/// when it does, this walker picks the new signal up for free.
fn collect_return_types(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    stmt_id: Idx<Stmt>,
    seen: &mut Option<TypeId>,
) -> Result<(), ()> {
    match &hir.stmts[stmt_id] {
        Stmt::Return(Some(e)) => {
            if let Some(ty) = analysis.expr_types.get(e).copied() {
                match *seen {
                    None => *seen = Some(ty),
                    Some(prev) => match join_return_types(arena, prev, ty) {
                        Some(joined) => *seen = Some(joined),
                        None => return Err(()),
                    },
                }
            }
            Ok(())
        }
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Breakpoint => Ok(()),
        Stmt::Block(b) => collect_returns_in_block(hir, analysis, arena, &b.stmts, seen),
        Stmt::If(i) => {
            collect_returns_in_block(hir, analysis, arena, &i.then_branch.stmts, seen)?;
            if let Some(eb) = i.else_branch {
                collect_return_types(hir, analysis, arena, eb, seen)?;
            }
            Ok(())
        }
        Stmt::While(w) => collect_returns_in_block(hir, analysis, arena, &w.body.stmts, seen),
        Stmt::DoWhile(w) => collect_returns_in_block(hir, analysis, arena, &w.body.stmts, seen),
        Stmt::For(f) => collect_returns_in_block(hir, analysis, arena, &f.body.stmts, seen),
        Stmt::ForIn(f) => collect_returns_in_block(hir, analysis, arena, &f.body.stmts, seen),
        Stmt::Try(t) => {
            collect_returns_in_block(hir, analysis, arena, &t.try_block.stmts, seen)?;
            collect_returns_in_block(hir, analysis, arena, &t.catch_block.stmts, seen)?;
            Ok(())
        }
        Stmt::At(a) => collect_returns_in_block(hir, analysis, arena, &a.block.stmts, seen),
        Stmt::Expr(_) | Stmt::Var(_) | Stmt::Assign(_) | Stmt::Throw(_) => Ok(()),
    }
}

/// Walk a block's statements in source order, descending into each
/// for its returns. Stops the iteration after the first statement that
/// terminates control flow — anything after it is dead code that
/// shouldn't contribute to the inferred return type.
///
/// The divergent statement itself IS visited (it might be a `return`
/// whose value we care about, or a containing `if` whose then-branch
/// returns). The cutoff is "siblings AFTER a divergent one".
fn collect_returns_in_block(
    hir: &Hir,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    stmts: &[Idx<Stmt>],
    seen: &mut Option<TypeId>,
) -> Result<(), ()> {
    for s in stmts {
        collect_return_types(hir, analysis, arena, *s, seen)?;
        if crate::reachability::stmt_diverges_with_analysis(hir, analysis, *s) {
            break;
        }
    }
    Ok(())
}

/// `true` when `ty` is something the user can write in a `type_ident`
/// slot. The GCL grammar accepts `[typeof] [Mod::]Name[<...>][?]`; any
/// shape that doesn't fit that template can't be the right-hand side
/// of an `infer-return-type` hint because the user can't act on it,
/// and shouldn't surface in a lambda's `ret: Some(T)` slot either
/// (would render as something un-writable).
///
/// Expressible: primitives, named types (`Type(item)`), `Enum`,
/// `Generic<...>` with expressible args, `TypeOf(inner)` with an
/// expressible inner, `GenericParam` (the user has the param name in
/// scope), `Null` (`null` is a valid type keyword), and `Any` (`any`
/// is too — the lint filters that one as uninformative; lambda body
/// inference accepts it as a legitimate `any` return).
///
/// Not expressible: `Never`, `Unresolved`, `Lambda` (no GCL literal
/// for function types beyond the `function` primitive — which the
/// analyzer never picks here because closures type as `Lambda`), and
/// `Union` (no `T | U` syntax).
pub fn is_expressible_type_ident(arena: &TypeArena, ty: TypeId) -> bool {
    let t = arena.get(ty);
    match &t.kind {
        TypeKind::Null
        | TypeKind::Any
        | TypeKind::Primitive(_)
        | TypeKind::Type(_)
        | TypeKind::Enum { .. }
        | TypeKind::GenericParam { .. } => true,
        TypeKind::Generic { args, .. } => args.iter().all(|a| is_expressible_type_ident(arena, *a)),
        TypeKind::TypeOf(inner) => is_expressible_type_ident(arena, *inner),
        TypeKind::Never
        | TypeKind::Unresolved { .. }
        | TypeKind::Lambda { .. }
        | TypeKind::Union { .. } => false,
    }
}
