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
            // P25.5
            text: self.text(node).into(),
            byte_range: node.byte_range(),
        })
    }

    /// Allocate an ident for a property-position node that may be
    /// either a plain `ident` or a quoted `string` (the grammar
    /// accepts `Foo::a` and `Foo::"a"` interchangeably for enum
    /// variant access). For the `string` form, store the unquoted
    /// fragment text so `member_uses` / variant lookups succeed
    /// without callers having to strip quotes.
    ///
    /// Use this when the call site flattens both forms to a bare
    /// `Idx<Ident>` (e.g. the `Expr::QualifiedStatic` chain).
    /// New call sites should prefer
    /// [`Self::alloc_property_name`], which preserves the syntactic
    /// form via [`PropertyName`].
    fn alloc_property_ident(&mut self, node: tree_sitter::Node<'_>) -> Idx<Ident> {
        if node.kind() == "string" {
            let mut c = node.walk();
            if let Some(frag) = node
                .named_children(&mut c)
                .find(|n| n.kind() == "string_fragment")
            {
                return self.hir.idents.alloc(Ident {
                    // P25.5
                    text: self.text(frag).into(),
                    byte_range: node.byte_range(),
                });
            }
        }
        self.alloc_ident(node)
    }

    /// Allocate a property-position node and tag the returned
    /// [`PropertyName`] with the syntactic form (`ident` vs quoted
    /// `string`). The decoded text and byte range are interned the
    /// same way as [`Self::alloc_property_ident`].
    fn alloc_property_name(&mut self, node: tree_sitter::Node<'_>) -> PropertyName {
        if node.kind() == "string" {
            PropertyName::String(self.alloc_property_ident(node))
        } else {
            PropertyName::Ident(self.alloc_ident(node))
        }
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

/// Collect annotations (`@expose("renamed")`, `@permission`, …) from
/// the `annotations` named child of a decl-level node. returns
/// `Annotation { name, args }` where `args` carries every
/// string-literal argument the source provided (other arg shapes are
/// dropped — call-site consumers we have today only read string args).
// P25.7
fn lower_annotations(
    cx: &LowerCtx,
    decl_node: tree_sitter::Node<'_>,
) -> smallvec::SmallVec<[Annotation; 1]> {
    let mut cursor = decl_node.walk();
    let Some(annots_node) = decl_node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "annotations")
    else {
        return smallvec::SmallVec::new();
    };
    let mut out = smallvec::SmallVec::new();
    let mut c2 = annots_node.walk();
    for ann in annots_node.named_children(&mut c2) {
        if ann.kind() != "annotation" {
            continue;
        }
        let mut c3 = ann.walk();
        let Some(ident) = ann.named_children(&mut c3).find(|n| n.kind() == "ident") else {
            continue;
        };
        // P25.6
        let name: smol_str::SmolStr = cx.text(ident).into();
        // P25.6 / P25.7
        let mut args: smallvec::SmallVec<[smol_str::SmolStr; 1]> = smallvec::SmallVec::new();
        let mut c4 = ann.walk();
        if let Some(args_node) = ann.named_children(&mut c4).find(|n| n.kind() == "args") {
            let mut c5 = args_node.walk();
            for a in args_node.named_children(&mut c5) {
                if a.kind() == "string"
                    && let Some(value) = string_literal_value(cx, a)
                {
                    args.push(value.into());
                }
            }
        }
        out.push(Annotation { name, args });
    }
    out
}

fn string_literal_value(cx: &LowerCtx, string_node: tree_sitter::Node<'_>) -> Option<String> {
    let mut cursor = string_node.walk();
    let mut value = String::new();
    for piece in string_node.named_children(&mut cursor) {
        if piece.kind() == "string_fragment" {
            value.push_str(cx.text(piece));
        }
    }
    Some(value)
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

// P25.7
fn lower_generics(
    cx: &mut LowerCtx,
    node: Option<tree_sitter::Node<'_>>,
) -> smallvec::SmallVec<[Idx<Ident>; 2]> {
    let Some(node) = node else {
        return smallvec::SmallVec::new();
    };
    let mut out = smallvec::SmallVec::new();
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
            if c.kind() != "enum_field" {
                continue;
            }
            // Grammar: `enum_field: (ident | string) optional( "(" _expr ")" )`.
            // The first named child is the field name (ident OR
            // quoted string for multi-word names like
            // `enum E { "str field" }`); any *subsequent* named child
            // is the optional value expression. The earlier
            // `find(|n| n.kind() == "ident")` lowering silently
            // dropped string-named variants from the HIR, so
            // `Foo::"str field"` access could never resolve.
            let mut walker = c.walk();
            let mut child_iter = c.named_children(&mut walker);
            let Some(name_node) = child_iter.next() else {
                continue;
            };
            if !matches!(name_node.kind(), "ident" | "string") {
                continue;
            }
            let value_node = child_iter.next();
            drop(child_iter);
            drop(walker);
            let nid = cx.alloc_property_ident(name_node);
            let value = value_node.and_then(|v| lower_expr(cx, v));
            fields.push(cx.hir.enum_fields.alloc(EnumField {
                name: nid,
                value,
                byte_range: c.byte_range(),
            }));
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
    let block = lower_block_inline(cx, node)?;
    Some(cx.hir.stmts.alloc(Stmt::Block(block)))
}

/// Same as [`lower_block`] but returns the [`BlockStmt`] directly
/// without allocating into the `stmts` arena. Body-bearing statements
/// (`If::then_branch`, `While::body`, …) embed the `BlockStmt` so
/// callers reach the curly-brace `byte_range` without going through
/// the arena.
fn lower_block_inline(
    cx: &mut LowerCtx,
    node: tree_sitter::Node<'_>,
) -> Option<crate::types::BlockStmt> {
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
    Some(crate::types::BlockStmt {
        stmts,
        byte_range: node.byte_range(),
    })
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
            // The grammar puts the type either as a direct `type_ident`
            // child (rare local-var shape) or wrapped in a
            // `type_decorator` (the canonical `var x: T` shape — see
            // grammar.js's `var_decl`). Accept either; `lower_type_ref`
            // handles both.
            let ty = node
                .named_children(&mut node.walk())
                .find(|c| matches!(c.kind(), "type_ident" | "type_decorator"))
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
                .and_then(|n| lower_block_inline(cx, n))?;
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
                .and_then(|n| lower_block_inline(cx, n))?;
            Stmt::While(WhileStmt {
                condition,
                body,
                byte_range: node.byte_range(),
            })
        }
        "do_while_stmt" => {
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_block_inline(cx, n))?;
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
                .and_then(|n| lower_block_inline(cx, n))?;
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
            // P17.2 — the grammar's `for_in_stmt` carries `sepBy2(",",
            // for_in_param)` (no field name on the params themselves)
            // plus a field-tagged `iterator: _expr` for the iterable
            // and `block: block` for the body. Previous lowering
            // misread `child_by_field_name("iterator")` as a param
            // wrapper and asked for `name` on it, dropping the whole
            // for-in via `?` short-circuit. Walk named children for
            // `for_in_param` nodes.
            let mut cursor = node.walk();
            let mut params: Vec<ForInParam> = Vec::new();
            for c in node.named_children(&mut cursor) {
                if c.kind() != "for_in_param" {
                    continue;
                }
                let Some(name_node) = c.child_by_field_name("name") else {
                    continue;
                };
                let name = cx.alloc_ident(name_node);
                let ty = c
                    .child_by_field_name("type")
                    .and_then(|t| lower_type_ref(cx, t));
                params.push(ForInParam { name, ty });
            }
            if params.is_empty() {
                return None;
            }
            // **P22.4** — the grammar carries TWO sibling fields here:
            // `iterator: $._expr` and `range: optional($.interval_expr)`.
            // When the source is `xs[from..to]`, tree-sitter often
            // resolves the ambiguity as `iterator: xs` + `range:
            // [from..to]` (interval_expr at prec 2 winning over
            // offset_expr at prec 13 in this slot). If we lower only
            // the iterator, the body's `from`/`to` references never
            // reach the resolver and the unused-local lint fires
            // false-positive. Fold the range into the iterator: the
            // semantic shape is `Offset(iterator, range)` regardless
            // of how the parser split it.
            let iter_node = node.child_by_field_name("iterator").ok_or(()).ok();
            let range_node = node.child_by_field_name("range");
            let range = match (iter_node, range_node) {
                (Some(iter), Some(rng)) => {
                    let recv = lower_expr(cx, iter)?;
                    let idx = lower_expr(cx, rng)?;
                    let span = iter.start_byte()..rng.end_byte();
                    cx.hir.exprs.alloc(Expr::Offset(OffsetExpr {
                        receiver: recv,
                        index: idx,
                        pre_optional: false,
                        post_optional: false,
                        byte_range: span,
                    }))
                }
                (Some(iter), None) => lower_expr(cx, iter)?,
                _ => return None,
            };
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_block_inline(cx, n))?;
            Stmt::ForIn(ForInStmt {
                params,
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
                .and_then(|n| lower_block_inline(cx, n))?;
            let error_param = node
                .child_by_field_name("error_param")
                .map(|n| cx.alloc_ident(n));
            let catch_block = node
                .child_by_field_name("catch_block")
                .and_then(|n| lower_block_inline(cx, n))?;
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
                .and_then(|n| lower_block_inline(cx, n))?;
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
        "number" | "char" | "false" | "true" | "null" | "this" | "iso8601" => {
            // `number` covers every numeric form (int/decimal/scientific/
            // suffixed). P13.3: dispatch typed-suffix literals to their
            // own `LiteralKind` so the analyzer doesn't have to inspect
            // raw text for `_time` / `_duration` / unit suffixes. Plain
            // numeric / `_f` float literals stay `LiteralKind::Number`
            // (float vs int dispatch happens in the analyzer via
            // `numeric_literal_kind`).
            let lit_kind = match kind {
                "number" => classify_number(cx, node),
                "char" => LiteralKind::Char,
                "false" | "true" => LiteralKind::Bool,
                "null" => LiteralKind::Null,
                "this" => LiteralKind::This,
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
            // P17.5 — walk every child in source order and capture
            // both the text fragments and the `${expr}` interpolation
            // expressions. Non-template strings lower to a single
            // `Lit` part; template strings produce alternating
            // `Lit`/`Interp` parts.
            let mut parts: Vec<StringPart> = Vec::new();
            let mut c = node.walk();
            for piece in node.named_children(&mut c) {
                match piece.kind() {
                    "string_fragment" | "string_escape_sequence" => {
                        parts.push(StringPart::Lit {
                            text: cx.text(piece).to_string(),
                            byte_range: piece.byte_range(),
                        });
                    }
                    "string_substitution" => {
                        if let Some(inner) = piece.child_by_field_name("_expr").or_else(|| {
                            // Grammar: `string_substitution: ${ _expr }`.
                            // `_expr` is hidden, so it has no field tag —
                            // walk named children for the inner expr.
                            let mut sc = piece.walk();
                            piece.named_children(&mut sc).find(|n| n.kind() != "")
                        }) && let Some(expr) = lower_expr(cx, inner)
                        {
                            parts.push(StringPart::Interp {
                                expr,
                                byte_range: piece.byte_range(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Expr::String(StringExpr {
                parts,
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
            let property = cx.alloc_property_name(prop);
            let (pre_optional, post_optional) = optional_flags_around(node, prop.id());
            Expr::Member(MemberExpr {
                receiver,
                property,
                pre_optional,
                post_optional,
                byte_range: node.byte_range(),
            })
        }
        "arrow_expr" => {
            let prop = node.child_by_field_name("property")?;
            let receiver =
                first_named_child_excluding(node, prop.id()).and_then(|n| lower_expr(cx, n))?;
            let property = cx.alloc_property_name(prop);
            let (pre_optional, post_optional) = optional_flags_around(node, prop.id());
            Expr::Arrow(MemberExpr {
                receiver,
                property,
                pre_optional,
                post_optional,
                byte_range: node.byte_range(),
            })
        }
        "static_expr" => {
            // P15.8 — chained `module::Type::name` shapes can't fit
            // the simple `StaticExpr { ty: TypeRef, property: Ident }`
            // shape because the head is itself a `static_expr`, not a
            // `type_ident`. Detect the chain and lower as
            // `Expr::QualifiedStatic { chain: Vec<Idx<Ident>> }` with
            // every segment as a flat ident.
            let prop = node.child_by_field_name("property")?;
            let head = first_named_child_excluding(node, prop.id())?;
            if head.kind() == "static_expr" {
                let mut chain = Vec::new();
                if !collect_static_chain_idents(cx, node, &mut chain) {
                    return None;
                }
                Expr::QualifiedStatic {
                    chain,
                    byte_range: node.byte_range(),
                }
            } else {
                let ty = lower_type_ref(cx, head)?;
                // The property can be either an `ident` (`Foo::a`) or a
                // quoted `string` (`Foo::"a"`); both forms are valid
                // enum-variant access syntax. The `PropertyName` enum
                // preserves the syntactic form for diagnostics and
                // formatter round-trips.
                let property = cx.alloc_property_name(prop);
                Expr::Static(StaticExpr {
                    ty,
                    property,
                    byte_range: node.byte_range(),
                })
            }
        }
        "offset_expr" => {
            // `recv[index]` — two `_expr` children. The grammar also
            // emits `optional` tokens (`?`) before / after the indexer
            // for the null-safe forms `a?[i]` / `a[i]?`. We classify
            // each `optional` named child by whether it precedes the
            // first `_expr` (no — it's always between the receiver and
            // `[`) or follows the index expression.
            let mut cursor = node.walk();
            let mut recv: Option<Idx<Expr>> = None;
            let mut idx: Option<Idx<Expr>> = None;
            let mut pre_optional = false;
            let mut post_optional = false;
            for c in node.named_children(&mut cursor) {
                match c.kind() {
                    "optional" => {
                        if recv.is_some() && idx.is_some() {
                            post_optional = true;
                        } else if recv.is_some() {
                            // Between receiver and `[` — but the index
                            // hasn't been seen yet. This is the
                            // `a?[i]` shape.
                            pre_optional = true;
                        }
                    }
                    _ if recv.is_none() => recv = lower_expr(cx, c),
                    _ if idx.is_none() => idx = lower_expr(cx, c),
                    _ => {}
                }
            }
            let recv = recv?;
            let idx = idx?;
            Expr::Offset(OffsetExpr {
                receiver: recv,
                index: idx,
                pre_optional,
                post_optional,
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
            // Grammar:
            //   object_expr        := type_ident (object_initializers | object_fields)
            //   object_initializers := "{" sepBy(",", _expr) "}"          // positional
            //   object_fields       := "{" sepBy(",", object_field) "}"   // named
            //   object_field        := name:_expr ":" value:_expr
            //
            // Bug fixed (P17.6 investigation): the previous lowering
            // looked for `object_field` children inside `object_initializers`
            // and never entered the `object_fields` branch at all. Both
            // forms ended up producing `fields = []`, dropping every
            // value expression from the HIR — which silenced the
            // resolver on every ident inside an object literal and
            // produced cascading `unused-local` / `unused-param` /
            // `unresolved name` false positives downstream.
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| lower_type_ref(cx, n));
            let mut fields = Vec::new();
            let mut walk = node.walk();
            for child in node.named_children(&mut walk) {
                match child.kind() {
                    "object_initializers" => {
                        let mut c = child.walk();
                        for value_node in child.named_children(&mut c) {
                            if let Some(value) = lower_expr(cx, value_node) {
                                fields.push(ObjectField {
                                    name: None,
                                    value,
                                    byte_range: value_node.byte_range(),
                                });
                            }
                        }
                    }
                    "object_fields" => {
                        let mut c = child.walk();
                        for of in child.named_children(&mut c) {
                            if of.kind() != "object_field" {
                                continue;
                            }
                            // `name` is graphed as `_expr` in the grammar
                            // but is conventionally a bare ident; only
                            // record the binding when it actually is one.
                            let name = of
                                .child_by_field_name("name")
                                .filter(|n| n.kind() == "ident")
                                .map(|n| cx.alloc_ident(n));
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
                    _ => {}
                }
            }
            Expr::Object(ObjectExpr {
                ty,
                fields,
                byte_range: node.byte_range(),
            })
        }
        // **P19.15** — `from..to` (and `from..` / `..to`) plus the
        // math-style `]from..to]` / `[from..to[` interval flatten
        // into one HIR shape. Bracket inclusivity isn't load-bearing
        // for typing.
        "range_expr" | "interval_expr" => {
            let from = node
                .child_by_field_name("from")
                .and_then(|n| lower_expr(cx, n));
            let to = node
                .child_by_field_name("to")
                .and_then(|n| lower_expr(cx, n));
            Expr::Range {
                from,
                to,
                byte_range: node.byte_range(),
            }
        }
        _ => Expr::Unsupported {
            kind: kind_to_static(kind),
            byte_range: node.byte_range(),
        },
    };
    Some(cx.hir.exprs.alloc(expr))
}

// P13.3
/// Classify a `number` CST node by its typed suffix.
///
/// Walks `(number_suffixed (number_int|number_decimal|number_scientific)
/// (number_suffix) ...)` and inspects each suffix lexeme:
/// - `time` → [`LiteralKind::Time`].
/// - any duration-unit suffix (`y`, `d`, `h`, `m`, `s`, `ms`, `us`,
///   `ns`, `min`, `sec`, `hour`, `day`, `week`, `month`, `year`) or
///   the explicit `_duration` form → [`LiteralKind::Duration`].
/// - everything else (including `_f` float, `_node*` casts, plain
///   numbers) → [`LiteralKind::Number`]. The analyzer's
///   `numeric_literal_kind` then dispatches `_f`/`.`/scientific
///   between `int` and `float`.
fn classify_number(cx: &LowerCtx, node: tree_sitter::Node<'_>) -> LiteralKind {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() != "number_suffixed" {
            continue;
        }
        let mut sc = child.walk();
        for sub in child.named_children(&mut sc) {
            if sub.kind() != "number_suffix" {
                continue;
            }
            let raw = cx.text(sub).trim_matches('_');
            if raw.eq_ignore_ascii_case("time") {
                return LiteralKind::Time;
            }
            if is_duration_suffix(raw) {
                return LiteralKind::Duration;
            }
        }
    }
    LiteralKind::Number
}

fn is_duration_suffix(s: &str) -> bool {
    matches!(
        s,
        "duration"
            | "y"
            | "d"
            | "h"
            | "m"
            | "s"
            | "ms"
            | "us"
            | "ns"
            | "min"
            | "sec"
            | "hour"
            | "day"
            | "week"
            | "month"
            | "year"
            | "minute"
            | "second"
            | "millisecond"
            | "microsecond"
            | "nanosecond"
    )
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

/// Walk `node`'s named children for `optional` siblings of the
/// property at `prop_id`. Returns `(pre_optional, post_optional)`:
/// `pre` is true when an `optional` token sits before the property
/// (`a?.b` / `a?->b`); `post` is true when one sits after
/// (`a.b?` / `a->b?`).
fn optional_flags_around(node: tree_sitter::Node<'_>, prop_id: usize) -> (bool, bool) {
    let mut cursor = node.walk();
    let mut pre = false;
    let mut post = false;
    let mut seen_prop = false;
    for c in node.named_children(&mut cursor) {
        if c.id() == prop_id {
            seen_prop = true;
            continue;
        }
        if c.kind() == "optional" {
            if seen_prop {
                post = true;
            } else {
                pre = true;
            }
        }
    }
    (pre, post)
}

// P15.8
/// Walk a chained `static_expr` node left-to-right and
/// alloc each segment's ident into the HIR's idents arena, pushing
/// the resulting `Idx<Ident>` into `out`. Returns `false` if any
/// segment's ident node is missing (a malformed chain).
///
/// For `runtime::Identity::create`, `out` ends up as
/// `[runtime, Identity, create]`.
///
/// The leftmost segment is wrapped in a `type_ident` (because the
/// grammar sets the head's first form to `type_ident`); subsequent
/// segments come from each `static_expr.property` ident.
fn collect_static_chain_idents(
    cx: &mut LowerCtx,
    node: tree_sitter::Node<'_>,
    out: &mut Vec<Idx<Ident>>,
) -> bool {
    if node.kind() != "static_expr" {
        return false;
    }
    let prop = match node.child_by_field_name("property") {
        Some(p) => p,
        None => return false,
    };
    let head = match first_named_child_excluding(node, prop.id()) {
        Some(h) => h,
        None => return false,
    };
    if head.kind() == "static_expr" {
        if !collect_static_chain_idents(cx, head, out) {
            return false;
        }
    } else if head.kind() == "type_ident" {
        let name_node = match head.child_by_field_name("name") {
            Some(n) => n,
            None => return false,
        };
        out.push(cx.alloc_ident(name_node));
    } else {
        return false;
    }
    // Trailing chain segment can be `ident` or quoted `string`
    // (`module::Foo::"a"` is valid enum-variant access).
    out.push(cx.alloc_property_ident(prop));
    true
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
        "*" => UnaryOp::Deref,
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
    // Collect every `type_ident` child (other than the head `name`) as
    // a generic param. The grammar emits one `params:`-tagged child per
    // arg (`Map<K, V>` yields two `params: type_ident` nodes), so we
    // can't rely on `child_by_field_name("params")` — that returns
    // only the first. Walking the named children directly captures
    // them all.
    let mut params = Vec::new();
    let mut cursor = inner.walk();
    for c in inner.named_children(&mut cursor) {
        if c.kind() == "type_ident"
            && c.id() != name_node.id()
            && let Some(tp) = lower_type_ref(cx, c)
        {
            params.push(tp);
        }
    }
    let optional = inner
        .named_children(&mut inner.walk())
        .any(|c| c.kind() == "optional");
    Some(cx.hir.type_refs.alloc(TypeRef {
        name,
        params,
        optional,
        // P18.1 — store the `type_ident`'s own byte range, not the
        // wrapper's (`attr_type` / `type_decorator` include the leading
        // `:` / annotation tokens). The TS reference's `dump-types`
        // emits `TypeIdent` records over the type_ident span; matching
        // that lets the parity oracle diff cleanly.
        byte_range: range_of(inner),
    }))
}

fn range_of(node: tree_sitter::Node<'_>) -> Range<usize> {
    node.byte_range()
}
