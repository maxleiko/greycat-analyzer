//! Inlay hint handler — consumes the cached `ModuleAnalysis` to emit
//! type hints for `var x = expr` (no declared type), return-type hints
//! for `fn` without `:T`, and parameter-name hints in call sites.

use greycat_analyzer_analysis::analyzer::AnalysisResult;
use greycat_analyzer_analysis::project::{ModuleAnalysis, ProjectAnalysis};
use greycat_analyzer_analysis::resolver::{Definition, Resolutions};
use greycat_analyzer_core::{SymbolTable, TypeId};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{BlockStmt, Decl, Stmt};
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel};

use crate::conv::{byte_to_position, position_to_byte};

/// LSP entry point for inlay hints — consumes the cached
/// [`ModuleAnalysis`] from [`ProjectAnalysis`] so the cross-module
/// fixup passes ( cross-module member typing,  call-on-
/// member return-type inference,  cross-module call return-type
/// inference) all flow through. Capabilities that re-run a single-
/// file `analyzer::analyze` would miss those — that's the bug we
/// kept hitting whenever new project-level inference landed.
///
/// Convention: every LSP handler in [`crate::server`] resolves the
/// owning project for the request URI via `Backend::project_for` and
/// calls one of these `*_with_project` variants against that project's
/// analysis. The legacy `(text, lib, root)` shims below stay for unit
/// tests / single-file CLI commands but they must never be reached
/// from a live LSP session.
pub fn inlay_hints_with_project(
    module: &ModuleAnalysis,
    project: &ProjectAnalysis,
    text: &str,
    range: &lsp_types::Range,
) -> Vec<InlayHint> {
    // Render types via the project's `display_type`, which prefixes
    // `<module>::` whenever the bare decl name is ambiguous across
    // modules — so the user reading `var f = b::Foo {};` sees
    // `: b::Foo`, not the misleading bare `: Foo`.
    let render: &dyn Fn(TypeId) -> String = &|ty| project.display_type(ty).to_string();
    inlay_hints_inner(module, project.symbols(), render, text, range)
}

/// Shared emitter — takes a type-rendering closure so the
/// project-aware path can prefix qualifiers while the single-file
/// shim falls back to the bare arena printer.
fn inlay_hints_inner(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    render_ty: &dyn Fn(TypeId) -> String,
    text: &str,
    range: &lsp_types::Range,
) -> Vec<InlayHint> {
    let hir = &module.hir;
    let resolutions = &module.resolutions;
    let analysis = &module.analysis;

    let hir_module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let want = (
        position_to_byte(text, range.start),
        position_to_byte(text, range.end),
    );

    let mut out = Vec::new();
    for decl_id in &hir_module.decls {
        if let Decl::Fn(fnd) = &hir.decls[*decl_id] {
            // P13.7: return-type hint when the fn has no declared
            // return type but the analyzer inferred one from the body.
            if fnd.return_type.is_none()
                && let Some(body) = fnd.body
                && let Some(ty) = inferred_fn_return(hir, analysis, body)
            {
                let name_range = &hir.idents[fnd.name].byte_range;
                // Anchor the hint right after the params `)` so it reads
                // `fn foo(): int` — anchoring at the fn name end would
                // print `fn foo: int()` which looks like the type belongs
                // to the name, not the return slot.
                let anchor = params_close_paren_end(text, name_range.end).unwrap_or(name_range.end);
                if name_range.start <= want.1 && anchor >= want.0 {
                    let label = format!(": {}", render_ty(ty));
                    out.push(InlayHint {
                        position: byte_to_position(text, anchor),
                        label: InlayHintLabel::String(label),
                        kind: Some(InlayHintKind::TYPE),
                        text_edits: None,
                        tooltip: None,
                        padding_left: None,
                        padding_right: None,
                        data: None,
                    });
                }
            }
            // Walk the body for `var name = expr;` shapes (no declared type).
            if let Some(body) = fnd.body {
                emit_var_hints(hir, analysis, render_ty, body, want, text, &mut out);
                // P13.7: argument-name hints inside the body.
                emit_call_arg_hints(hir, symbols, resolutions, body, want, text, &mut out);
            }
        }
    }
    out
}

/// Scan forward from the fn name's end to find the byte offset
/// immediately after the params list's closing `)`. Tracks paren
/// depth so a nested `(T)` inside a type annotation doesn't fool the
/// scan. Returns `None` if the close paren can't be found (parse
/// error / truncated source).
fn params_close_paren_end(text: &str, after_name: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = after_name;
    while i < bytes.len() && bytes[i] != b'(' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let mut depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// P13.7
/// Peek at the last expression-shaped statement of a fn body
/// to infer its return type. Returns `None` for blocks that don't end
/// in a `Stmt::Return(...)` with an inferred-type expression.
fn inferred_fn_return(hir: &Hir, analysis: &AnalysisResult, body: Idx<Stmt>) -> Option<TypeId> {
    let block = match &hir.stmts[body] {
        Stmt::Block(b) => b,
        _ => return None,
    };
    for s in block.stmts.iter().rev() {
        if let Stmt::Return(Some(e)) = &hir.stmts[*s] {
            return analysis.expr_types.get(e).copied();
        }
    }
    None
}

/// Same as [`emit_call_arg_hints`] but recurses into a `BlockStmt`
/// directly, since body-bearing fields (`If::then_branch`, …) hold
/// the block inline now.
fn emit_call_arg_hints_block(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    block: &BlockStmt,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_call_arg_hints(hir, symbols, resolutions, *s, want, text, out);
    }
}

// P13.7
/// Walk the body for `Expr::Call` and emit one
/// `<param_name>:` hint anchored at the start of each positional arg.
fn emit_call_arg_hints(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    stmt_id: Idx<Stmt>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => emit_call_arg_hints_block(hir, symbols, resolutions, b, want, text, out),
        Stmt::Expr(e)
        | Stmt::Return(Some(e))
        | Stmt::Throw(e)
        | Stmt::Var(greycat_analyzer_hir::types::LocalVar { init: Some(e), .. }) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *e, want, text, out);
        }
        Stmt::Assign(a) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, a.target, want, text, out);
            emit_call_arg_hints_expr(hir, symbols, resolutions, a.value, want, text, out);
        }
        Stmt::If(i) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, i.condition, want, text, out);
            emit_call_arg_hints_block(hir, symbols, resolutions, &i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_call_arg_hints(hir, symbols, resolutions, eb, want, text, out);
            }
        }
        Stmt::While(w) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, w.condition, want, text, out);
            emit_call_arg_hints_block(hir, symbols, resolutions, &w.body, want, text, out);
        }
        Stmt::DoWhile(w) => {
            emit_call_arg_hints_block(hir, symbols, resolutions, &w.body, want, text, out);
            emit_call_arg_hints_expr(hir, symbols, resolutions, w.condition, want, text, out);
        }
        Stmt::For(f) => {
            emit_call_arg_hints_block(hir, symbols, resolutions, &f.body, want, text, out)
        }
        Stmt::ForIn(f) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, f.range, want, text, out);
            emit_call_arg_hints_block(hir, symbols, resolutions, &f.body, want, text, out);
        }
        Stmt::Try(t) => {
            emit_call_arg_hints_block(hir, symbols, resolutions, &t.try_block, want, text, out);
            emit_call_arg_hints_block(hir, symbols, resolutions, &t.catch_block, want, text, out);
        }
        Stmt::At(a) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, a.expr, want, text, out);
            emit_call_arg_hints_block(hir, symbols, resolutions, &a.block, want, text, out);
        }
        _ => {}
    }
}

fn emit_call_arg_hints_expr(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    expr_id: Idx<greycat_analyzer_hir::types::Expr>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    use greycat_analyzer_hir::types::{CallExpr, Expr};
    match &hir.exprs[expr_id] {
        Expr::Call(CallExpr { callee, args, .. }) => {
            // Recurse into nested args first so hints fire on inner
            // calls too.
            emit_call_arg_hints_expr(hir, symbols, resolutions, *callee, want, text, out);
            for a in args {
                emit_call_arg_hints_expr(hir, symbols, resolutions, *a, want, text, out);
            }
            // Look up callee's params.
            if let Expr::Ident { name: name_idx, .. } = &hir.exprs[*callee]
                && let Some(Definition::Decl(decl_id)) = resolutions.lookup(*name_idx)
                && let Decl::Fn(fnd) = &hir.decls[decl_id]
            {
                for (i, arg) in args.iter().enumerate() {
                    let Some(p_id) = fnd.params.get(i) else {
                        break;
                    };
                    let p = &hir.fn_params[*p_id];
                    let param_name = symbols[hir.idents[p.name].symbol].to_string();
                    if param_name.starts_with('_') {
                        continue;
                    }
                    let arg_range = hir.exprs[*arg].byte_range();
                    if arg_range.start > want.1 || arg_range.end < want.0 {
                        continue;
                    }
                    out.push(InlayHint {
                        position: byte_to_position(text, arg_range.start),
                        label: InlayHintLabel::String(format!("{param_name}:")),
                        kind: Some(InlayHintKind::PARAMETER),
                        text_edits: None,
                        tooltip: None,
                        padding_left: None,
                        padding_right: Some(true),
                        data: None,
                    });
                }
            }
        }
        Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for e in items {
                emit_call_arg_hints_expr(hir, symbols, resolutions, *e, want, text, out);
            }
        }
        Expr::Member(m) | Expr::Arrow(m) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, m.receiver, want, text, out);
        }
        Expr::Offset(o) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, o.receiver, want, text, out);
            emit_call_arg_hints_expr(hir, symbols, resolutions, o.index, want, text, out);
        }
        Expr::Binary(b) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, b.left, want, text, out);
            emit_call_arg_hints_expr(hir, symbols, resolutions, b.right, want, text, out);
        }
        Expr::Unary(u) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, u.operand, want, text, out)
        }
        Expr::Paren(inner, _) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *inner, want, text, out)
        }
        Expr::Object(o) => {
            for f in &o.fields {
                emit_call_arg_hints_expr(hir, symbols, resolutions, f.value, want, text, out);
            }
        }
        Expr::Lambda(l) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, l.body, want, text, out)
        }
        Expr::Is { value, .. } | Expr::Cast { value, .. } => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *value, want, text, out);
        }
        _ => {}
    }
}

/// Walk a `BlockStmt` recursively for var-hint emission. Body-bearing
/// statements hold the block inline post-refactor so we can't go via
/// Push a `: T` type hint anchored at `name_end`. Caller is responsible
/// for the want-range overlap check and rendering the type to a string.
fn push_type_hint(text: &str, name_end: usize, rendered_ty: String, out: &mut Vec<InlayHint>) {
    out.push(InlayHint {
        position: byte_to_position(text, name_end),
        label: InlayHintLabel::String(format!(": {rendered_ty}")),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: None,
        padding_right: None,
        data: None,
    });
}

/// `Idx<Stmt>` for them.
fn emit_var_hints_block(
    hir: &Hir,
    analysis: &AnalysisResult,
    render_ty: &dyn Fn(TypeId) -> String,
    block: &BlockStmt,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_var_hints(hir, analysis, render_ty, *s, want, text, out);
    }
}

fn emit_var_hints(
    hir: &Hir,
    analysis: &AnalysisResult,
    render_ty: &dyn Fn(TypeId) -> String,
    stmt_id: Idx<Stmt>,
    want: (usize, usize),
    text: &str,
    out: &mut Vec<InlayHint>,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => emit_var_hints_block(hir, analysis, render_ty, b, want, text, out),
        Stmt::Var(v) if v.ty.is_none() && v.init.is_some() => {
            let r = &v.byte_range;
            if r.end < want.0 || r.start > want.1 {
                return;
            }
            let init_id = v.init.unwrap();
            let Some(ty) = analysis.expr_types.get(&init_id).copied() else {
                return;
            };
            let label = format!(": {}", render_ty(ty));
            // Anchor right after the variable name.
            let name_range = &hir.idents[v.name].byte_range;
            out.push(InlayHint {
                position: byte_to_position(text, name_range.end),
                label: InlayHintLabel::String(label),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            });
        }
        Stmt::If(i) => {
            emit_var_hints_block(hir, analysis, render_ty, &i.then_branch, want, text, out);
            if let Some(eb) = i.else_branch {
                emit_var_hints(hir, analysis, render_ty, eb, want, text, out);
            }
        }
        Stmt::While(w) => emit_var_hints_block(hir, analysis, render_ty, &w.body, want, text, out),
        Stmt::DoWhile(w) => {
            emit_var_hints_block(hir, analysis, render_ty, &w.body, want, text, out)
        }
        Stmt::For(f) => {
            // C-style for: `for (var i = 0; …)`. Emit `: T` after the
            // init name when there's no declared annotation and the
            // analyzer settled a type on it (`def_types[init_name]`,
            // bound in P19.14).
            if f.init_ty.is_none()
                && let Some(name) = f.init_name
                && let Some(ty) = analysis.def_types.get(&name).copied()
            {
                let name_range = &hir.idents[name].byte_range;
                if name_range.end >= want.0 && name_range.start <= want.1 {
                    push_type_hint(text, name_range.end, render_ty(ty), out);
                }
            }
            emit_var_hints_block(hir, analysis, render_ty, &f.body, want, text, out);
        }
        Stmt::ForIn(f) => {
            // `for (k, v in arr)`. Emit `: T` after each un-annotated
            // param name using `def_types[p.name]` — bound by the
            // analyzer's @iterable element-type machinery (P18.x) or
            // the declared annotation when present.
            for p in f.params.iter() {
                if p.ty.is_some() {
                    continue;
                }
                let Some(ty) = analysis.def_types.get(&p.name).copied() else {
                    continue;
                };
                let name_range = &hir.idents[p.name].byte_range;
                if name_range.end < want.0 || name_range.start > want.1 {
                    continue;
                }
                push_type_hint(text, name_range.end, render_ty(ty), out);
            }
            emit_var_hints_block(hir, analysis, render_ty, &f.body, want, text, out);
        }
        Stmt::Try(t) => {
            emit_var_hints_block(hir, analysis, render_ty, &t.try_block, want, text, out);
            emit_var_hints_block(hir, analysis, render_ty, &t.catch_block, want, text, out);
        }
        Stmt::At(a) => emit_var_hints_block(hir, analysis, render_ty, &a.block, want, text, out),
        _ => {}
    }
}
