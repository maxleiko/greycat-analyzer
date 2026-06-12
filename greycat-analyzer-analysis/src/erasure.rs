//! Function-generic erasure classification.
//!
//! The GreyCat runtime does not monomorphize function-level generic
//! parameters: a `.gcl`-bodied `fn f<T>(...)` is compiled once with `T`
//! erased to `any?`. So when such a fn *constructs and returns* a
//! container parameterized by `T` (`Array<T>{}`, `Tuple<Array<T>, int>{}`,
//! …) the value the runtime produces is `Array<any?>` /
//! `Tuple<Array<any?>, int>` — the analyzer's call-site materialization
//! (`Array<Person>` / `Tuple<Array<Person>, int>`) is more specific than
//! reality. Passing that value into a more-specifically-parameterized
//! slot then *throws at runtime* (verified against `greycat run`):
//! `argument of type 'Array' is not assignable to parameter ... 'Array<Person>'`.
//!
//! Not every generic fn erases its result:
//! - A **bare-`T`** return (`fn first<T>(a: Array<T>): T`) hands back the
//!   actual value, which keeps its real class — no erasure.
//! - A **pass-through** gcl body (`fn id<T>(x: Array<T>): Array<T> { return x; }`)
//!   forwards whatever it received — no *new* erasure.
//! - A **native** fn honors `T` whenever it can recover it from a
//!   parameter (`clone<T>(v: T)`, `min`/`max`, `enum_by_offset<T>(t: typeof T)`).
//!   Only `native fn make<T>(): T` (T unrecoverable, return-only) erases —
//!   and the runtime can't produce a concrete `T` there at all.
//! - **node-family** results (`node<T>`, `nodeList<T>`) erase too, but a
//!   node is a `u64` handle with no stored type arg, so the runtime never
//!   *checks* it on assignment — node-tag bivariance in
//!   `is_assignable_to_with_index` lets the erased form flow into any
//!   `node<T>` slot. That self-exclusion lives at the assignability
//!   check, not here: this module still reports node-returning ctors as
//!   erasing; the diagnostic simply never fires on them.
//!
//! This module answers one question per fn: **does calling it yield a
//! runtime-erased container result?** It is deliberately conservative —
//! it only reports `true` when erasure is *provable*, so the
//! `generic-erasure` diagnostic (Error severity) never false-positives.
//! Misses (e.g. a result laundered through an opaque call) stay silent.

use greycat_analyzer_core::Symbol;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Expr, FnDecl, Stmt, TypeRef};

/// Var-trace recursion budget — guards against pathological
/// self-referential `var x = x;` chains while comfortably covering the
/// `var a = ctor; return a;` shape this targets.
const TRACE_FUEL: u8 = 8;

/// Whether a call to `fnd` produces a value whose container type
/// argument(s) the runtime will have erased to `any?`. See the module
/// note; conservative by design (only `true` when provable).
pub(crate) fn fn_result_erases(hir: &Hir, fnd: &FnDecl) -> bool {
    if fnd.generics.is_empty() {
        return false;
    }
    let Some(ret) = fnd.return_type else {
        return false;
    };
    let generics: Vec<Symbol> = fnd.generics.iter().map(|g| hir.idents[*g].symbol).collect();
    // Only a generic sitting *inside* a container arg slot erases. A bare
    // `T` / `T?` return passes the value through with its real class, so
    // it never throws on narrowing.
    if !type_ref_has_generic_in_arg(hir, ret, &generics) {
        return false;
    }
    match fnd.body {
        // native: honored whenever `T` is recoverable from a parameter.
        // Conservatively, if `T` appears in ANY param position assume the
        // impl honors it (never false-positive on stdlib natives). The
        // only erasing native shape is `T` solely in the return
        // (`make<T>(): T`) — nothing to recover it from.
        None => !fnd.params.iter().any(|p_id| {
            hir.fn_params[*p_id]
                .ty
                .is_some_and(|tr| type_ref_mentions_generic(hir, tr, &generics))
        }),
        // gcl: the compiler erases `Container<T>{}` construction. The
        // result erases iff the returned value provably traces to such a
        // constructor.
        Some(body) => {
            let mut returns: Vec<Idx<Expr>> = Vec::new();
            collect_return_values(hir, body, &mut returns);
            returns
                .iter()
                .any(|r| expr_traces_to_erasing_ctor(hir, *r, body, &generics, TRACE_FUEL))
        }
    }
}

/// `true` iff one of `generics` appears as a type *argument* nested
/// inside `ty` (the `T` in `Array<T>` / `Tuple<Array<T>, int>`), as
/// opposed to `ty` being the bare generic itself (`T` / `T?`).
fn type_ref_has_generic_in_arg(hir: &Hir, ty: Idx<TypeRef>, generics: &[Symbol]) -> bool {
    hir.type_refs[ty]
        .params
        .iter()
        .any(|p| type_ref_mentions_generic(hir, *p, generics))
}

/// `true` iff `ty` or any nested arg names one of `generics`.
fn type_ref_mentions_generic(hir: &Hir, ty: Idx<TypeRef>, generics: &[Symbol]) -> bool {
    let tr = &hir.type_refs[ty];
    if tr.qualifier.is_empty() && generics.contains(&hir.idents[tr.name].symbol) {
        return true;
    }
    tr.params
        .iter()
        .any(|p| type_ref_mentions_generic(hir, *p, generics))
}

/// Does `expr` — directly, or after tracing through local `var`
/// initializers — construct a container whose type mentions a generic?
fn expr_traces_to_erasing_ctor(
    hir: &Hir,
    expr: Idx<Expr>,
    body: Idx<Stmt>,
    generics: &[Symbol],
    fuel: u8,
) -> bool {
    if fuel == 0 {
        return false;
    }
    match &hir.exprs[expr] {
        Expr::Object(o) => type_ref_has_generic_in_arg(hir, o.ty, generics),
        Expr::PositionalObject(o) => type_ref_has_generic_in_arg(hir, o.ty, generics),
        Expr::Paren(inner, _) => expr_traces_to_erasing_ctor(hir, *inner, body, generics, fuel - 1),
        Expr::Ident { name, .. } => {
            let sym = hir.idents[*name].symbol;
            find_local_var_init(hir, body, sym).is_some_and(|init| {
                expr_traces_to_erasing_ctor(hir, init, body, generics, fuel - 1)
            })
        }
        _ => false,
    }
}

/// Collect every `return <expr>;` value reachable from `stmt`,
/// descending into nested blocks / branches / loops. Mirrors the
/// statement descent in [`crate::return_inference`].
fn collect_return_values(hir: &Hir, stmt: Idx<Stmt>, out: &mut Vec<Idx<Expr>>) {
    match &hir.stmts[stmt] {
        Stmt::Return(r) => {
            if let Some(v) = r.value {
                out.push(v);
            }
        }
        Stmt::Block(b) => b
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::If(i) => {
            i.then_branch
                .stmts
                .iter()
                .for_each(|s| collect_return_values(hir, *s, out));
            if let Some(eb) = i.else_branch {
                collect_return_values(hir, eb, out);
            }
        }
        Stmt::While(w) => w
            .body
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::DoWhile(w) => w
            .body
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::For(f) => f
            .body
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::ForIn(f) => f
            .body
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::Try(t) => {
            t.try_block
                .stmts
                .iter()
                .for_each(|s| collect_return_values(hir, *s, out));
            t.catch_block
                .stmts
                .iter()
                .for_each(|s| collect_return_values(hir, *s, out));
        }
        Stmt::At(a) => a
            .block
            .stmts
            .iter()
            .for_each(|s| collect_return_values(hir, *s, out)),
        Stmt::Expr(_)
        | Stmt::Var(_)
        | Stmt::Assign(_)
        | Stmt::Throw(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::Breakpoint(_) => {}
    }
}

/// First local `var <sym> = init;` initializer declared in `stmt`
/// (descending into nested blocks). Good enough for the single-
/// assignment `var x = ctor; return x;` shape the tracer targets.
fn find_local_var_init(hir: &Hir, stmt: Idx<Stmt>, sym: Symbol) -> Option<Idx<Expr>> {
    match &hir.stmts[stmt] {
        Stmt::Var(v) if hir.idents[v.name].symbol == sym => v.init,
        Stmt::Var(_) => None,
        Stmt::Block(b) => b
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::If(i) => i
            .then_branch
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym))
            .or_else(|| {
                i.else_branch
                    .and_then(|eb| find_local_var_init(hir, eb, sym))
            }),
        Stmt::While(w) => w
            .body
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::DoWhile(w) => w
            .body
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::For(f) => f
            .body
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::ForIn(f) => f
            .body
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::Try(t) => t
            .try_block
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym))
            .or_else(|| {
                t.catch_block
                    .stmts
                    .iter()
                    .find_map(|s| find_local_var_init(hir, *s, sym))
            }),
        Stmt::At(a) => a
            .block
            .stmts
            .iter()
            .find_map(|s| find_local_var_init(hir, *s, sym)),
        Stmt::Expr(_)
        | Stmt::Assign(_)
        | Stmt::Return(_)
        | Stmt::Throw(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::Breakpoint(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_core::SymbolTable;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_hir::types::Decl;
    use greycat_analyzer_syntax::parse;

    /// Classify the first top-level `fn` decl in `src`.
    fn erases(src: &str) -> bool {
        let tree = parse(src);
        let symbols = SymbolTable::default();
        let hir = lower_module(src, &symbols, "mod", "project", tree.root_node());
        let fnd = hir
            .decls
            .iter()
            .find_map(|(_, d)| match d {
                Decl::Fn(fnd) => Some(fnd),
                _ => None,
            })
            .expect("a top-level fn decl");
        fn_result_erases(&hir, fnd)
    }

    #[test]
    fn direct_ctor_return_erases() {
        // The user's example shape: returns a freshly-built container
        // whose type mentions the fn generic.
        assert!(erases(
            "fn make<T>(x: T): Tuple<Array<T>, int> { return Tuple<Array<T>, int> { x: x, y: 1 }; }\n"
        ));
    }

    #[test]
    fn var_traced_ctor_return_erases() {
        // `var a = Array<T>{}; ...; return a;` — trace the var initializer.
        assert!(erases(
            "fn wrap<T>(x: T): Array<T> { var a = Array<T> {}; a.add(x); return a; }\n"
        ));
    }

    #[test]
    fn bare_generic_return_does_not_erase() {
        // The value passes through with its real class.
        assert!(!erases(
            "fn first<T>(a: Array<T>): T { return a.get(0); }\n"
        ));
    }

    #[test]
    fn param_passthrough_does_not_erase() {
        assert!(!erases("fn id<T>(x: Array<T>): Array<T> { return x; }\n"));
    }

    #[test]
    fn var_aliased_param_does_not_erase() {
        // `var b = paramArray; return b;` traces back to a param, not a
        // constructor.
        assert!(!erases(
            "fn id<T>(x: Array<T>): Array<T> { var b = x; return b; }\n"
        ));
    }

    #[test]
    fn native_with_generic_in_param_does_not_erase() {
        // The native impl can recover `T` from `v`.
        assert!(!erases("native fn dup<T>(v: T): Array<T>;\n"));
    }

    #[test]
    fn native_with_generic_return_only_erases() {
        // `make<T>(): Array<T>` — nothing to recover `T` from.
        assert!(erases("native fn make<T>(): Array<T>;\n"));
    }

    #[test]
    fn nongeneric_fn_does_not_erase() {
        assert!(!erases(
            "fn build(): Array<int> { return Array<int> {}; }\n"
        ));
    }
}
