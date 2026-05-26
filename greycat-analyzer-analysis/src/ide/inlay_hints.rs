//! Inlay hints — consumes the cached `ModuleAnalysis` to emit type
//! hints for `var x = expr` (no declared type), return-type hints for
//! `fn` without `:T`, and parameter-name hints in call sites.
//!
//! Returns IDE-shape [`InlayHint`] values; the LSP server's
//! `capabilities/inlay_hints.rs` converts to `lsp_types::InlayHint` at
//! the wire boundary.

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

use greycat_analyzer_core::conv::byte_to_position;
use greycat_analyzer_core::{SourceEncoding, SymbolTable, TypeId};
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{BlockStmt, Decl, Stmt};

use crate::analyzer::AnalysisResult;
use crate::conv::position_to_byte;
use crate::ide::types::{Position, Range};
use crate::project::{ModuleAnalysis, ProjectAnalysis};
use crate::resolver::{Definition, Resolutions};

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlayHintKind {
    Type,
    Parameter,
}

#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone)]
pub struct InlayHint {
    pub position: Position,
    #[cfg_attr(feature = "wasm", wasm_bindgen(getter_with_clone))]
    pub label: String,
    pub kind: InlayHintKind,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// Project-aware inlay hints — consumes the cached [`ModuleAnalysis`]
/// so cross-module fixup passes (member typing, call-on-member return-
/// type inference, cross-module call return-type inference) all flow
/// through.
pub fn inlay_hints_with_project(
    module: &ModuleAnalysis,
    project: &ProjectAnalysis,
    text: &str,
    range: &Range,
    encoding: SourceEncoding,
) -> Vec<InlayHint> {
    let render: &dyn Fn(TypeId) -> String = &|ty| project.display_type(ty).to_string();
    inlay_hints_inner(module, project.symbols(), render, text, range, encoding)
}

fn inlay_hints_inner(
    module: &ModuleAnalysis,
    symbols: &SymbolTable,
    render_ty: &dyn Fn(TypeId) -> String,
    text: &str,
    range: &Range,
    encoding: SourceEncoding,
) -> Vec<InlayHint> {
    let hir = &module.hir;
    let resolutions = &module.resolutions;
    let analysis = &module.analysis;

    let hir_module = match hir.module.as_ref() {
        Some(m) => m,
        None => return Vec::new(),
    };

    let want = (
        pos_to_byte(text, range.start, encoding),
        pos_to_byte(text, range.end, encoding),
    );

    let mut out = Vec::new();
    for decl_id in &hir_module.decls {
        if let Decl::Fn(fnd) = &hir.decls[*decl_id] {
            // Return-type hint when the fn has no declared return type
            // but the analyzer inferred one from the body.
            if fnd.return_type.is_none()
                && let Some(body) = fnd.body
                && let Some(ty) = inferred_fn_return(hir, analysis, body)
            {
                let name_range = &hir.idents[fnd.name].byte_range;
                // Anchor the hint right after the params `)` so it
                // reads `fn foo(): int`.
                let anchor = params_close_paren_end(text, name_range.end).unwrap_or(name_range.end);
                if name_range.start <= want.1 && anchor >= want.0 {
                    let label = format!(": {}", render_ty(ty));
                    out.push(InlayHint {
                        position: byte_to_ide_pos(text, anchor, encoding),
                        label,
                        kind: InlayHintKind::Type,
                        padding_left: false,
                        padding_right: false,
                    });
                }
            }
            if let Some(body) = fnd.body {
                emit_var_hints(
                    hir, analysis, render_ty, body, want, text, encoding, &mut out,
                );
                emit_call_arg_hints(
                    hir,
                    symbols,
                    resolutions,
                    body,
                    want,
                    text,
                    encoding,
                    &mut out,
                );
            }
        }
    }
    out
}

fn pos_to_byte(text: &str, p: Position, encoding: SourceEncoding) -> usize {
    position_to_byte(
        text,
        greycat_analyzer_core::lsp_types::Position {
            line: p.line,
            character: p.character,
        },
        encoding,
    )
}

fn byte_to_ide_pos(text: &str, byte: usize, encoding: SourceEncoding) -> Position {
    let p = byte_to_position(text, byte, encoding);
    Position {
        line: p.line,
        character: p.character,
    }
}

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

#[allow(clippy::too_many_arguments)]
fn emit_call_arg_hints_block(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    block: &BlockStmt,
    want: (usize, usize),
    text: &str,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_call_arg_hints(hir, symbols, resolutions, *s, want, text, encoding, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_call_arg_hints(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    stmt_id: Idx<Stmt>,
    want: (usize, usize),
    text: &str,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => {
            emit_call_arg_hints_block(hir, symbols, resolutions, b, want, text, encoding, out)
        }
        Stmt::Expr(e)
        | Stmt::Return(Some(e))
        | Stmt::Throw(e)
        | Stmt::Var(greycat_analyzer_hir::types::LocalVar { init: Some(e), .. }) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *e, want, text, encoding, out);
        }
        Stmt::Assign(a) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                a.target,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                a.value,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::If(i) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                i.condition,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &i.then_branch,
                want,
                text,
                encoding,
                out,
            );
            if let Some(eb) = i.else_branch {
                emit_call_arg_hints(hir, symbols, resolutions, eb, want, text, encoding, out);
            }
        }
        Stmt::While(w) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                w.condition,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &w.body,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::DoWhile(w) => {
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &w.body,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                w.condition,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::For(f) => emit_call_arg_hints_block(
            hir,
            symbols,
            resolutions,
            &f.body,
            want,
            text,
            encoding,
            out,
        ),
        Stmt::ForIn(f) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                f.range,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &f.body,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::Try(t) => {
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &t.try_block,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &t.catch_block,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::At(a) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, a.expr, want, text, encoding, out);
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &a.block,
                want,
                text,
                encoding,
                out,
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_call_arg_hints_expr(
    hir: &Hir,
    symbols: &SymbolTable,
    resolutions: &Resolutions,
    expr_id: Idx<greycat_analyzer_hir::types::Expr>,
    want: (usize, usize),
    text: &str,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    use greycat_analyzer_hir::types::{CallExpr, Expr};
    match &hir.exprs[expr_id] {
        Expr::Call(CallExpr { callee, args, .. }) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                *callee,
                want,
                text,
                encoding,
                out,
            );
            for a in args {
                emit_call_arg_hints_expr(hir, symbols, resolutions, *a, want, text, encoding, out);
            }
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
                        position: byte_to_ide_pos(text, arg_range.start, encoding),
                        label: format!("{param_name}:"),
                        kind: InlayHintKind::Parameter,
                        padding_left: false,
                        padding_right: true,
                    });
                }
            }
        }
        Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for e in items {
                emit_call_arg_hints_expr(hir, symbols, resolutions, *e, want, text, encoding, out);
            }
        }
        Expr::Member(m) | Expr::Arrow(m) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                m.receiver,
                want,
                text,
                encoding,
                out,
            );
        }
        Expr::Offset(o) => {
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                o.receiver,
                want,
                text,
                encoding,
                out,
            );
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                o.index,
                want,
                text,
                encoding,
                out,
            );
        }
        Expr::Binary(b) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, b.left, want, text, encoding, out);
            emit_call_arg_hints_expr(
                hir,
                symbols,
                resolutions,
                b.right,
                want,
                text,
                encoding,
                out,
            );
        }
        Expr::Unary(u) => emit_call_arg_hints_expr(
            hir,
            symbols,
            resolutions,
            u.operand,
            want,
            text,
            encoding,
            out,
        ),
        Expr::Paren(inner, _) => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *inner, want, text, encoding, out)
        }
        Expr::Object(o) => {
            for f in &o.fields {
                emit_call_arg_hints_expr(
                    hir,
                    symbols,
                    resolutions,
                    f.value,
                    want,
                    text,
                    encoding,
                    out,
                );
            }
        }
        Expr::Lambda(l) => {
            emit_call_arg_hints_block(
                hir,
                symbols,
                resolutions,
                &l.body,
                want,
                text,
                encoding,
                out,
            );
        }
        Expr::Is { value, .. } | Expr::Cast { value, .. } => {
            emit_call_arg_hints_expr(hir, symbols, resolutions, *value, want, text, encoding, out);
        }
        _ => {}
    }
}

fn push_type_hint(
    text: &str,
    name_end: usize,
    rendered_ty: String,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    out.push(InlayHint {
        position: byte_to_ide_pos(text, name_end, encoding),
        label: format!(": {rendered_ty}"),
        kind: InlayHintKind::Type,
        padding_left: false,
        padding_right: false,
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_var_hints_block(
    hir: &Hir,
    analysis: &AnalysisResult,
    render_ty: &dyn Fn(TypeId) -> String,
    block: &BlockStmt,
    want: (usize, usize),
    text: &str,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    for s in &block.stmts {
        emit_var_hints(hir, analysis, render_ty, *s, want, text, encoding, out);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_var_hints(
    hir: &Hir,
    analysis: &AnalysisResult,
    render_ty: &dyn Fn(TypeId) -> String,
    stmt_id: Idx<Stmt>,
    want: (usize, usize),
    text: &str,
    encoding: SourceEncoding,
    out: &mut Vec<InlayHint>,
) {
    let stmt = &hir.stmts[stmt_id];
    match stmt {
        Stmt::Block(b) => {
            emit_var_hints_block(hir, analysis, render_ty, b, want, text, encoding, out)
        }
        Stmt::Var(v) if v.ty.is_none() && v.init.is_some() => {
            let r = &v.byte_range;
            if r.end < want.0 || r.start > want.1 {
                return;
            }
            let init_id = v.init.unwrap();
            let Some(ty) = analysis.expr_types.get(&init_id).copied() else {
                return;
            };
            let name_range = &hir.idents[v.name].byte_range;
            push_type_hint(text, name_range.end, render_ty(ty), encoding, out);
        }
        Stmt::If(i) => {
            emit_var_hints_block(
                hir,
                analysis,
                render_ty,
                &i.then_branch,
                want,
                text,
                encoding,
                out,
            );
            if let Some(eb) = i.else_branch {
                emit_var_hints(hir, analysis, render_ty, eb, want, text, encoding, out);
            }
        }
        Stmt::While(w) => {
            emit_var_hints_block(hir, analysis, render_ty, &w.body, want, text, encoding, out)
        }
        Stmt::DoWhile(w) => {
            emit_var_hints_block(hir, analysis, render_ty, &w.body, want, text, encoding, out)
        }
        Stmt::For(f) => {
            if f.init_ty.is_none()
                && let Some(name) = f.init_name
                && let Some(ty) = analysis.def_types.get(&name).copied()
            {
                let name_range = &hir.idents[name].byte_range;
                if name_range.end >= want.0 && name_range.start <= want.1 {
                    push_type_hint(text, name_range.end, render_ty(ty), encoding, out);
                }
            }
            emit_var_hints_block(hir, analysis, render_ty, &f.body, want, text, encoding, out);
        }
        Stmt::ForIn(f) => {
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
                push_type_hint(text, name_range.end, render_ty(ty), encoding, out);
            }
            emit_var_hints_block(hir, analysis, render_ty, &f.body, want, text, encoding, out);
        }
        Stmt::Try(t) => {
            emit_var_hints_block(
                hir,
                analysis,
                render_ty,
                &t.try_block,
                want,
                text,
                encoding,
                out,
            );
            emit_var_hints_block(
                hir,
                analysis,
                render_ty,
                &t.catch_block,
                want,
                text,
                encoding,
                out,
            );
        }
        Stmt::At(a) => emit_var_hints_block(
            hir, analysis, render_ty, &a.block, want, text, encoding, out,
        ),
        _ => {}
    }
}
