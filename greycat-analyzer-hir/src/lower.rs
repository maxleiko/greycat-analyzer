//! Tree-sitter CST → HIR lowering. Walks named children, plucks
//! field-keyed sub-nodes, and pushes typed records into the [`Hir`]
//! arenas. Tolerant: unknown / not-yet-lowered shapes become
//! [`Expr::Unsupported`] / are skipped, never panics.

use std::ops::Range;

use greycat_analyzer_syntax::tree_sitter;

use crate::Hir;
use crate::arena::Idx;
use crate::types::*;

pub struct LowerCtx<'src> {
    pub hir: Hir,
    source: &'src str,
}

impl<'src> LowerCtx<'src> {
    pub fn new(source: &'src str) -> Self {
        Self {
            hir: Hir::default(),
            source,
        }
    }

    fn text(&self, node: tree_sitter::Node<'_>) -> &'src str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    fn alloc_ident(&mut self, node: tree_sitter::Node<'_>) -> Idx<Ident> {
        self.hir.idents.alloc(Ident {
            text: self.text(node).to_string(),
            byte_range: node.byte_range(),
        })
    }
}

pub fn lower_module(
    source: &str,
    name: impl Into<String>,
    lib: impl Into<String>,
    root: tree_sitter::Node<'_>,
) -> Hir {
    let mut cx = LowerCtx::new(source);
    let mut decl_ids: Vec<Idx<Decl>> = Vec::new();

    if root.kind() == "module" {
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if let Some(d) = lower_decl(&mut cx, child) {
                decl_ids.push(d);
            }
        }
    }

    cx.hir.module = Some(Module {
        name: name.into(),
        lib: lib.into(),
        decls: decl_ids,
        byte_range: root.byte_range(),
    });
    cx.hir
}

// =============================================================================
// Declarations
// =============================================================================

fn lower_decl(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Decl>> {
    match node.kind() {
        "fn_decl" => {
            let fnd = lower_fn_decl(cx, node)?;
            Some(cx.hir.decls.alloc(Decl::Fn(fnd)))
        }
        "type_decl" => {
            let td = lower_type_decl(cx, node)?;
            Some(cx.hir.decls.alloc(Decl::Type(td)))
        }
        "enum_decl" => {
            let ed = lower_enum_decl(cx, node)?;
            Some(cx.hir.decls.alloc(Decl::Enum(ed)))
        }
        "modvar" => {
            let v = lower_top_var(cx, node)?;
            Some(cx.hir.decls.alloc(Decl::Var(v)))
        }
        "mod_pragma" => {
            let p = lower_pragma(cx, node)?;
            Some(cx.hir.decls.alloc(Decl::Pragma(p)))
        }
        _ => None,
    }
}

fn lower_modifiers(cx: &LowerCtx, node: Option<tree_sitter::Node<'_>>) -> Modifiers {
    let mut m = Modifiers::default();
    let Some(node) = node else { return m };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match cx.text(child) {
            "private" => m.private = true,
            "static" => m.static_ = true,
            "abstract" => m.abstract_ = true,
            "native" => m.native = true,
            _ => {}
        }
    }
    m
}

/// Collect annotation names (e.g. `["expose", "permission"]`) from the
/// `annotations` named child of a decl-level node. Args are dropped —
/// downstream consumers (P6.7 unused-decl lint) only need the bare name.
fn lower_annotations(cx: &LowerCtx, decl_node: tree_sitter::Node<'_>) -> Vec<String> {
    let mut cursor = decl_node.walk();
    let Some(annots_node) = decl_node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "annotations")
    else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut c2 = annots_node.walk();
    for ann in annots_node.named_children(&mut c2) {
        if ann.kind() != "annotation" {
            continue;
        }
        let mut c3 = ann.walk();
        if let Some(ident) = ann.named_children(&mut c3).find(|n| n.kind() == "ident") {
            names.push(cx.text(ident).to_string());
        }
    }
    names
}

fn doc_text(cx: &LowerCtx, node: tree_sitter::Node<'_>) -> Option<String> {
    let doc = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "doc")?;
    let mut s = String::new();
    let mut cursor = doc.walk();
    for c in doc.named_children(&mut cursor) {
        if c.kind() == "doc_comment" {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(cx.text(c).trim_start_matches("///").trim());
        }
    }
    Some(s)
}

fn lower_fn_decl(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<FnDecl> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    let generics = lower_generics(cx, node.child_by_field_name("generics"));
    let params = lower_fn_params(cx, node.child_by_field_name("params"));
    let return_type = node
        .child_by_field_name("return_type")
        .and_then(|n| lower_type_ref(cx, n));
    let body = node
        .child_by_field_name("body")
        .and_then(|b| lower_block(cx, b));
    let doc = doc_text(cx, node);

    Some(FnDecl {
        name,
        modifiers,
        generics,
        params,
        return_type,
        body,
        doc,
        byte_range: node.byte_range(),
    })
}

fn lower_generics(cx: &mut LowerCtx, node: Option<tree_sitter::Node<'_>>) -> Vec<Idx<Ident>> {
    let Some(node) = node else { return Vec::new() };
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if c.kind() == "ident" {
            out.push(cx.alloc_ident(c));
        }
    }
    out
}

fn lower_fn_params(cx: &mut LowerCtx, node: Option<tree_sitter::Node<'_>>) -> Vec<Idx<FnParam>> {
    let Some(node) = node else { return Vec::new() };
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if c.kind() == "fn_param" {
            let Some(name_node) = c.child_by_field_name("name") else {
                continue;
            };
            let name = cx.alloc_ident(name_node);
            let ty = c
                .child_by_field_name("type")
                .and_then(|n| lower_type_ref(cx, n));
            let param = cx.hir.fn_params.alloc(FnParam {
                name,
                ty,
                byte_range: c.byte_range(),
            });
            out.push(param);
        }
    }
    out
}

fn lower_type_decl(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<TypeDecl> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    let generics = lower_generics(cx, node.child_by_field_name("params"));
    let supertype = node
        .child_by_field_name("supertype")
        .and_then(|n| lower_type_ref(cx, n));

    let mut attrs = Vec::new();
    let mut methods = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "type_attr" => {
                    if let Some(a) = lower_type_attr(cx, child) {
                        attrs.push(cx.hir.type_attrs.alloc(a));
                    }
                }
                "type_method" => {
                    // Same shape as fn_decl for our purposes.
                    if let Some(fnd) = lower_fn_decl(cx, child) {
                        methods.push(cx.hir.decls.alloc(Decl::Fn(fnd)));
                    }
                }
                _ => {}
            }
        }
    }

    Some(TypeDecl {
        name,
        modifiers,
        generics,
        supertype,
        attrs,
        methods,
        doc: doc_text(cx, node),
        byte_range: node.byte_range(),
    })
}

fn lower_type_attr(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<TypeAttr> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    let ty = node
        .child_by_field_name("type")
        .and_then(|n| lower_type_ref(cx, n));
    let init = node.child_by_field_name("init").and_then(|n| {
        // attr_init wraps an expression
        let mut cursor = n.walk();
        let inner = n.named_children(&mut cursor).next()?;
        lower_expr(cx, inner)
    });
    Some(TypeAttr {
        name,
        modifiers,
        ty,
        init,
        doc: doc_text(cx, node),
        byte_range: node.byte_range(),
    })
}

fn lower_enum_decl(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<EnumDecl> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for c in body.named_children(&mut cursor) {
            if c.kind() == "enum_field"
                && let Some(field_name) = c
                    .named_children(&mut c.walk())
                    .find(|n| n.kind() == "ident")
            {
                let nid = cx.alloc_ident(field_name);
                let value = c
                    .named_children(&mut c.walk())
                    .find(|n| n.kind() != "ident")
                    .and_then(|v| lower_expr(cx, v));
                fields.push(cx.hir.enum_fields.alloc(EnumField {
                    name: nid,
                    value,
                    byte_range: c.byte_range(),
                }));
            }
        }
    }
    Some(EnumDecl {
        name,
        modifiers,
        fields,
        doc: doc_text(cx, node),
        byte_range: node.byte_range(),
    })
}

fn lower_top_var(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<VarDeclTop> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    let ty = node
        .child_by_field_name("type")
        .and_then(|n| lower_type_ref(cx, n));
    // var_decl has no field for init; modvar has an `initializer` child.
    let init = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "initializer")
        .and_then(|i| {
            i.child_by_field_name("expr")
                .and_then(|e| lower_expr(cx, e))
        });
    Some(VarDeclTop {
        name,
        modifiers,
        ty,
        init,
        byte_range: node.byte_range(),
    })
}

fn lower_pragma(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Pragma> {
    // mod_pragma: doc? annotation _semi
    let annotation = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "annotation")?;
    let mut name: Option<Idx<Ident>> = None;
    let mut args: Vec<Idx<Expr>> = Vec::new();
    let mut cursor = annotation.walk();
    for c in annotation.named_children(&mut cursor) {
        if c.kind() == "ident" && name.is_none() {
            name = Some(cx.alloc_ident(c));
        } else if c.kind() == "args" {
            let mut ac = c.walk();
            for arg in c.named_children(&mut ac) {
                if let Some(e) = lower_expr(cx, arg) {
                    args.push(e);
                }
            }
        }
    }
    Some(Pragma {
        name: name?,
        args,
        byte_range: node.byte_range(),
    })
}

// =============================================================================
// Statements
// =============================================================================

fn lower_block(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Stmt>> {
    if node.kind() != "block" {
        return None;
    }
    let mut stmts = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if let Some(s) = lower_stmt(cx, c) {
            stmts.push(s);
        }
    }
    Some(cx.hir.stmts.alloc(Stmt::Block(stmts)))
}

fn lower_stmt(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Stmt>> {
    let stmt = match node.kind() {
        "block" => return lower_block(cx, node),
        "expr_stmt" => {
            // expr_stmt wraps a single expression child
            let expr = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|e| lower_expr(cx, e))?;
            Stmt::Expr(expr)
        }
        "var_decl" => {
            let name_node = node.child_by_field_name("name")?;
            let name = cx.alloc_ident(name_node);
            // The grammar puts `: T` on a sibling without a field; the type
            // ident node, if present, is the second `type_ident` child.
            let ty = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "type_ident")
                .and_then(|n| lower_type_ref(cx, n));
            let init = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "initializer")
                .and_then(|i| i.child_by_field_name("expr"))
                .and_then(|e| lower_expr(cx, e));
            Stmt::Var(LocalVar {
                name,
                ty,
                init,
                byte_range: node.byte_range(),
            })
        }
        "if_stmt" => {
            let condition = node
                .child_by_field_name("condition")
                .and_then(|n| lower_expr(cx, n))?;
            let then_branch = node
                .child_by_field_name("then_branch")
                .and_then(|n| lower_stmt(cx, n))?;
            // The grammar's `_else_branch` is a hidden rule, so field
            // annotations sometimes don't propagate to the inner
            // if_stmt / block. Fall back to scanning the if_stmt's
            // named children for the second `block` or any `if_stmt`
            // (the first block is the then_branch, the second — if
            // present — is the else_branch).
            let else_branch = node
                .child_by_field_name("else_branch")
                .and_then(|n| lower_stmt(cx, n))
                .or_else(|| {
                    let then_id = node.child_by_field_name("then_branch")?.id();
                    let mut cursor = node.walk();
                    let mut seen_then = false;
                    for c in node.named_children(&mut cursor) {
                        if c.id() == then_id {
                            seen_then = true;
                            continue;
                        }
                        if !seen_then {
                            continue;
                        }
                        match c.kind() {
                            "block" | "if_stmt" => return lower_stmt(cx, c),
                            _ => {}
                        }
                    }
                    None
                });
            Stmt::If(IfStmt {
                condition,
                then_branch,
                else_branch,
                byte_range: node.byte_range(),
            })
        }
        "while_stmt" => {
            let condition = node
                .child_by_field_name("condition")
                .and_then(|n| lower_expr(cx, n))?;
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_stmt(cx, n))?;
            Stmt::While(WhileStmt {
                condition,
                body,
                byte_range: node.byte_range(),
            })
        }
        "do_while_stmt" => {
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_stmt(cx, n))?;
            let condition = node
                .child_by_field_name("condition")
                .and_then(|n| lower_expr(cx, n))?;
            Stmt::DoWhile(DoWhileStmt {
                body,
                condition,
                byte_range: node.byte_range(),
            })
        }
        "for_stmt" => {
            let init_name = node
                .child_by_field_name("it_name")
                .map(|n| cx.alloc_ident(n));
            let init_ty = node
                .child_by_field_name("it_type")
                .and_then(|n| lower_type_ref(cx, n));
            let init_value = node
                .child_by_field_name("it_value")
                .and_then(|n| lower_expr(cx, n));
            let condition = node
                .child_by_field_name("it_condition")
                .and_then(|n| lower_expr(cx, n));
            let increment = node
                .child_by_field_name("it_increment")
                .and_then(|n| lower_expr(cx, n));
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_stmt(cx, n))?;
            Stmt::For(ForStmt {
                init_name,
                init_ty,
                init_value,
                condition,
                increment,
                body,
                byte_range: node.byte_range(),
            })
        }
        "for_in_stmt" => {
            let iter_param = node.child_by_field_name("iterator")?;
            let iter_name_node = iter_param.child_by_field_name("name")?;
            let iterator_name = cx.alloc_ident(iter_name_node);
            let iterator_type = iter_param
                .child_by_field_name("type")
                .and_then(|t| lower_type_ref(cx, t));
            let range = node
                .child_by_field_name("range")
                .and_then(|r| lower_expr(cx, r))?;
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_stmt(cx, n))?;
            Stmt::ForIn(ForInStmt {
                iterator_name,
                iterator_type,
                range,
                body,
                byte_range: node.byte_range(),
            })
        }
        "return_stmt" => {
            let value = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|e| lower_expr(cx, e));
            Stmt::Return(value)
        }
        "break_stmt" => Stmt::Break,
        "continue_stmt" => Stmt::Continue,
        "throw_stmt" => {
            let e = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|x| lower_expr(cx, x))?;
            Stmt::Throw(e)
        }
        "try_stmt" => {
            let try_block = node
                .child_by_field_name("try_block")
                .and_then(|n| lower_stmt(cx, n))?;
            let error_param = node
                .child_by_field_name("error_param")
                .and_then(|n| n.child_by_field_name("name"))
                .map(|n| cx.alloc_ident(n));
            let catch_block = node
                .child_by_field_name("catch_block")
                .and_then(|n| lower_stmt(cx, n))?;
            Stmt::Try(TryStmt {
                try_block,
                error_param,
                catch_block,
                byte_range: node.byte_range(),
            })
        }
        "at_stmt" => {
            let expr = node
                .child_by_field_name("expr")
                .and_then(|n| lower_expr(cx, n))?;
            let block = node
                .child_by_field_name("block")
                .and_then(|n| lower_stmt(cx, n))?;
            Stmt::At(AtStmt {
                expr,
                block,
                byte_range: node.byte_range(),
            })
        }
        _ => return None,
    };
    Some(cx.hir.stmts.alloc(stmt))
}

// =============================================================================
// Expressions
// =============================================================================

fn lower_expr(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Expr>> {
    let kind = node.kind();
    let expr = match kind {
        "ident" => {
            let id = cx.alloc_ident(node);
            Expr::Ident(id)
        }
        "number" | "char" | "false" | "true" | "null" | "this" | "duration" | "iso8601" => {
            let lit_kind = match kind {
                "number" => LiteralKind::Number,
                "char" => LiteralKind::Char,
                "false" | "true" => LiteralKind::Bool,
                "null" => LiteralKind::Null,
                "this" => LiteralKind::This,
                "duration" => LiteralKind::Duration,
                "iso8601" => LiteralKind::Iso8601,
                _ => unreachable!(),
            };
            Expr::Literal(LiteralExpr {
                kind: lit_kind,
                text: cx.text(node).to_string(),
                byte_range: node.byte_range(),
            })
        }
        "string" => {
            let mut value = String::new();
            let mut c = node.walk();
            for piece in node.named_children(&mut c) {
                if piece.kind() == "string_fragment" {
                    value.push_str(cx.text(piece));
                }
            }
            Expr::String(StringExpr {
                value,
                byte_range: node.byte_range(),
            })
        }
        "tuple_expr" => {
            let parts = lower_expr_list(cx, node);
            Expr::Tuple(parts, node.byte_range())
        }
        "array_expr" => {
            let parts = lower_expr_list(cx, node);
            Expr::Array(parts, node.byte_range())
        }
        "paren_expr" => {
            let inner = node
                .child_by_field_name("expr")
                .and_then(|e| lower_expr(cx, e))?;
            Expr::Paren(inner, node.byte_range())
        }
        "member_expr" => {
            let prop = node.child_by_field_name("property")?;
            let receiver =
                first_named_child_excluding(node, prop.id()).and_then(|n| lower_expr(cx, n))?;
            let property = cx.alloc_ident(prop);
            Expr::Member(MemberExpr {
                receiver,
                property,
                byte_range: node.byte_range(),
            })
        }
        "arrow_expr" => {
            let prop = node.child_by_field_name("property")?;
            let receiver =
                first_named_child_excluding(node, prop.id()).and_then(|n| lower_expr(cx, n))?;
            let property = cx.alloc_ident(prop);
            Expr::Arrow(MemberExpr {
                receiver,
                property,
                byte_range: node.byte_range(),
            })
        }
        "static_expr" => {
            let prop = node.child_by_field_name("property")?;
            let ty_node = first_named_child_excluding(node, prop.id())?;
            let ty = lower_type_ref(cx, ty_node)?;
            let property = cx.alloc_ident(prop);
            Expr::Static(StaticExpr {
                ty,
                property,
                byte_range: node.byte_range(),
            })
        }
        "offset_expr" => {
            // `recv[index]` — two named children (recv, index).
            let mut cursor = node.walk();
            let mut iter = node.named_children(&mut cursor);
            let recv = iter.next().and_then(|n| lower_expr(cx, n))?;
            let idx = iter.next().and_then(|n| lower_expr(cx, n))?;
            Expr::Offset(OffsetExpr {
                receiver: recv,
                index: idx,
                byte_range: node.byte_range(),
            })
        }
        "call_expr" => {
            let callee = node
                .child_by_field_name("fn")
                .and_then(|n| lower_expr(cx, n))?;
            let args = node
                .named_children(&mut node.walk())
                .find(|n| n.kind() == "args")
                .map(|a| lower_expr_list(cx, a))
                .unwrap_or_default();
            Expr::Call(CallExpr {
                callee,
                args,
                byte_range: node.byte_range(),
            })
        }
        "binary_expr" => {
            // P6.5: `is` / `as` use a `type_ident` on the right rather
            // than another expr. Detect them here and emit dedicated
            // HIR variants instead of forcing them through Binary.
            let op_text = operator_text(cx, node);
            if op_text == "is" || op_text == "as" {
                let value = node
                    .child_by_field_name("left")
                    .and_then(|n| lower_expr(cx, n))?;
                let ty = node
                    .child_by_field_name("right")
                    .and_then(|n| lower_type_ref(cx, n))?;
                let br = node.byte_range();
                return Some(cx.hir.exprs.alloc(if op_text == "is" {
                    Expr::Is {
                        value,
                        ty,
                        byte_range: br,
                    }
                } else {
                    Expr::Cast {
                        value,
                        ty,
                        byte_range: br,
                    }
                }));
            }

            let left = node
                .child_by_field_name("left")
                .and_then(|n| lower_expr(cx, n))?;
            let right = node
                .child_by_field_name("right")
                .and_then(|n| lower_expr(cx, n))?;
            let op = bin_op_for(op_text);
            Expr::Binary(BinaryExpr {
                op,
                left,
                right,
                byte_range: node.byte_range(),
            })
        }
        "unary_expr" => {
            let operand = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|n| lower_expr(cx, n))?;
            let op = unary_op_for(operator_text(cx, node));
            Expr::Unary(UnaryExpr {
                op,
                operand,
                byte_range: node.byte_range(),
            })
        }
        "lambda_expr" => {
            let params = lower_fn_params(cx, node.child_by_field_name("params"));
            let body = node
                .child_by_field_name("body")
                .and_then(|n| lower_expr(cx, n))?;
            Expr::Lambda(LambdaExpr {
                params,
                body,
                byte_range: node.byte_range(),
            })
        }
        "object_expr" => {
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| lower_type_ref(cx, n));
            let mut fields = Vec::new();
            if let Some(inits) = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "object_initializers")
            {
                let mut c = inits.walk();
                for of in inits.named_children(&mut c) {
                    if of.kind() != "object_field" {
                        continue;
                    }
                    let name = of.child_by_field_name("name").map(|n| cx.alloc_ident(n));
                    let value = of
                        .child_by_field_name("value")
                        .and_then(|v| lower_expr(cx, v));
                    if let Some(value) = value {
                        fields.push(ObjectField {
                            name,
                            value,
                            byte_range: of.byte_range(),
                        });
                    }
                }
            }
            Expr::Object(ObjectExpr {
                ty,
                fields,
                byte_range: node.byte_range(),
            })
        }
        _ => Expr::Unsupported {
            kind: kind_to_static(kind),
            byte_range: node.byte_range(),
        },
    };
    Some(cx.hir.exprs.alloc(expr))
}

fn lower_expr_list(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Vec<Idx<Expr>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if let Some(e) = lower_expr(cx, c) {
            out.push(e);
        }
    }
    out
}

fn first_named_child_excluding<'tree>(
    node: tree_sitter::Node<'tree>,
    excluded_id: usize,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|c| c.id() != excluded_id)
}

fn operator_text<'src>(cx: &LowerCtx<'src>, node: tree_sitter::Node<'_>) -> &'src str {
    // The operator is the first anonymous (non-named) child.
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if !c.is_named() {
            return cx.text(c);
        }
    }
    ""
}

fn bin_op_for(text: &str) -> BinOp {
    match text {
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        "%" => BinOp::Mod,
        "==" => BinOp::Eq,
        "!=" => BinOp::Neq,
        "<" => BinOp::Lt,
        "<=" => BinOp::Lte,
        ">" => BinOp::Gt,
        ">=" => BinOp::Gte,
        "&&" => BinOp::And,
        "||" => BinOp::Or,
        "&" => BinOp::BitAnd,
        "|" => BinOp::BitOr,
        "^" => BinOp::BitXor,
        "<<" => BinOp::Shl,
        ">>" => BinOp::Shr,
        "??" => BinOp::Coalesce,
        other => BinOp::Other(static_str(other)),
    }
}

fn unary_op_for(text: &str) -> UnaryOp {
    match text {
        "-" => UnaryOp::Neg,
        "!" => UnaryOp::Not,
        "~" => UnaryOp::BitNot,
        "!!" => UnaryOp::NonNullAssert,
        _ => UnaryOp::Not,
    }
}

/// Tree-sitter kind strings are themselves `&'static str`s embedded in the
/// generated parser tables, but we can't carry a tree-sitter borrow across
/// arena allocations. Intern via a leak — the set of node kinds is bounded
/// (~70) so the leak is bounded too.
fn kind_to_static(kind: &str) -> &'static str {
    Box::leak(kind.to_string().into_boxed_str())
}

fn static_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

// =============================================================================
// Type references
// =============================================================================

fn lower_type_ref(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<TypeRef>> {
    // The grammar wraps actual ids in `type_ident` (with field `name` and
    // optional `params`), but `attr_type`, `type_decorator`, etc. embed it.
    let inner = match node.kind() {
        "type_ident" => node,
        "attr_type" | "type_decorator" => node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "type_ident")?,
        _ => node,
    };
    if inner.kind() != "type_ident" {
        return None;
    }
    let name_node = inner.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut params = Vec::new();
    if let Some(p) = inner.child_by_field_name("params") {
        if let Some(t) = lower_type_ref(cx, p) {
            params.push(t);
        } else {
            // params is itself a type_ident in tree-sitter, but for a multi-
            // param case it should iterate. Walk siblings if needed.
            let mut cursor = inner.walk();
            for c in inner.named_children(&mut cursor) {
                if c.kind() == "type_ident"
                    && c.id() != name_node.id()
                    && let Some(tp) = lower_type_ref(cx, c)
                {
                    params.push(tp);
                }
            }
        }
    }
    let optional = inner
        .named_children(&mut inner.walk())
        .any(|c| c.kind() == "optional");
    Some(cx.hir.type_refs.alloc(TypeRef {
        name,
        params,
        optional,
        byte_range: range_of(node),
    }))
}

fn range_of(node: tree_sitter::Node<'_>) -> Range<usize> {
    node.byte_range()
}
