//! Tree-sitter CST → HIR lowering. Walks named children and pushes
//! typed records into the [`Hir`] arenas. Tolerant: unknown shapes
//! become [`Expr::Unsupported`] or are skipped; never panics.

use std::ops::Range;

use greycat_analyzer_core::{Symbol, SymbolTable};
use greycat_analyzer_syntax::{cst, tree_sitter};

use crate::Hir;
use crate::arena::Idx;
use crate::hir::*;

pub struct LowerCtx<'src, 'symbols> {
    pub hir: Hir,
    source: &'src str,
    symbols: &'symbols SymbolTable,
}

impl<'src, 'symbols> LowerCtx<'src, 'symbols> {
    pub fn new(source: &'src str, symbols: &'symbols SymbolTable) -> Self {
        Self {
            hir: Hir::default(),
            source,
            symbols,
        }
    }

    fn text(&self, node: tree_sitter::Node<'_>) -> &'src str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    fn alloc_ident(&mut self, node: tree_sitter::Node<'_>) -> Idx<Ident> {
        self.hir.idents.alloc(Ident {
            symbol: self.symbols.intern(self.text(node)),
            byte_range: node.byte_range(),
        })
    }

    /// Allocate an ident for a property-position node that may be a
    /// plain `ident` or a quoted `string` (`Foo::a` and `Foo::"a"` are
    /// interchangeable for enum-variant access). The `string` form
    /// stores the unquoted fragment text. Use when the call site
    /// flattens both forms to a bare `Idx<Ident>`; prefer
    /// [`Self::alloc_property_name`] otherwise.
    fn alloc_property_ident(&mut self, node: tree_sitter::Node<'_>) -> Idx<Ident> {
        if node.kind() == "string" {
            let mut c = node.walk();
            if let Some(frag) = node
                .named_children(&mut c)
                .find(|n| n.kind() == "string_fragment")
            {
                return self.hir.idents.alloc(Ident {
                    symbol: self.symbols.intern(self.text(frag)),
                    byte_range: node.byte_range(),
                });
            }
        }
        self.alloc_ident(node)
    }

    /// Allocate a property-position node, tagging the [`PropertyName`]
    /// with its syntactic form (`ident` vs quoted `string`).
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
    symbols: &SymbolTable,
    name: &str,
    lib: &str,
    root: tree_sitter::Node<'_>,
) -> Hir {
    let mut cx = LowerCtx::new(source, symbols);
    let mut decl_ids: Vec<Idx<Decl>> = Vec::new();

    if root.kind() == "module" {
        // Salvage helper so a mid-edit decl wrapped in an
        // `ERROR` still produces an `Idx<Decl>` when its inner shape is
        // recognizable.
        for (child, _salvaged) in flatten_errors_named_children(root) {
            if let Some(d) = lower_decl(&mut cx, child) {
                decl_ids.push(d);
            }
        }
    }

    cx.hir.module = Some(Module {
        name: symbols.intern(name),
        lib: symbols.intern(lib),
        decls: decl_ids.into_boxed_slice(),
        byte_range: root.byte_range(),
    });
    cx.hir
}

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
            let v = lower_modvar(cx, node)?;
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

/// Collect annotations (`@expose("renamed")`, `@tag("mcp")`,
/// `@timeout(5s)`, …) from a decl's `annotations` child. One
/// [`Annotation`] per `annotation` CST child; `args` carries every
/// primitive-literal arg in source order.
// P25.7
fn lower_annotations(cx: &LowerCtx, decl_node: tree_sitter::Node<'_>) -> Box<[Annotation]> {
    let mut cursor = decl_node.walk();
    let Some(annots_node) = decl_node
        .named_children(&mut cursor)
        .find(|c| c.kind() == "annotations")
    else {
        return Box::default();
    };
    let mut out: Vec<Annotation> = Vec::new();
    let mut c2 = annots_node.walk();
    for ann in annots_node.named_children(&mut c2) {
        if ann.kind() != "annotation" {
            continue;
        }
        let mut c3 = ann.walk();
        let Some(ident) = ann.named_children(&mut c3).find(|n| n.kind() == "ident") else {
            continue;
        };
        let name = Ident {
            symbol: cx.symbols.intern(cx.text(ident)),
            byte_range: ident.byte_range(),
        };
        let mut args: Vec<AnnotationArg> = Vec::new();
        let mut c4 = ann.walk();
        if let Some(args_node) = ann.named_children(&mut c4).find(|n| n.kind() == "args") {
            let mut c5 = args_node.walk();
            for a in args_node.named_children(&mut c5) {
                args.push(AnnotationArg {
                    kind: lower_annotation_arg(cx, a),
                    span: a.byte_range(),
                });
            }
        }
        out.push(Annotation {
            name,
            args: args.into_boxed_slice(),
        });
    }
    out.into_boxed_slice()
}

/// Lower a single annotation arg into an [`AnnotationArgKind`] (the
/// caller wraps it with the source span). String args are interned.
/// Non-literal arg nodes become [`AnnotationArgKind::Invalid`] (hard
/// `invalid-pragma-arg` error).
fn lower_annotation_arg(cx: &LowerCtx, node: tree_sitter::Node<'_>) -> AnnotationArgKind {
    match node.kind() {
        "string" => match string_literal_value(cx, node) {
            Some(value) => AnnotationArgKind::String(cx.symbols.intern(&value)),
            None => AnnotationArgKind::Invalid,
        },
        "true" => AnnotationArgKind::Bool(true),
        "false" => AnnotationArgKind::Bool(false),
        "null" => AnnotationArgKind::Null,
        "number" => {
            let raw = cx.text(node);
            let (kind, _issue) = classify_and_parse_number(cx, node, raw, false);
            lit_to_annotation_arg(kind)
        }
        "char" => {
            // The grammar nests `iso8601` inside `'...'` quotes.
            let iso_child = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "iso8601");
            let (kind, _issue) = match iso_child {
                Some(iso) => parse_iso8601(cx.text(iso)),
                None => parse_char(cx.text(node)),
            };
            lit_to_annotation_arg(kind)
        }
        // Path-shaped args — type pointers / enum-variant
        // references / bare names. The validator resolves them
        // against the project's name tables and rejects ones that
        // don't point at a type / enum / variant.
        "ident" | "type_ident" => {
            let chain = vec![cx.symbols.intern(cx.text(node))].into_boxed_slice();
            AnnotationArgKind::Path { chain }
        }
        "static_expr" => match collect_path_chain(cx, node) {
            Some(chain) => AnnotationArgKind::Path { chain },
            None => AnnotationArgKind::Invalid,
        },
        _ => AnnotationArgKind::Invalid,
    }
}

/// Collect a `static_expr`'s path segments as interned [`Symbol`]s.
/// `static_expr` is left-recursive (`(static_expr | type_ident) ::
/// property`), so recurse into the head and append the property last.
/// `None` if any segment isn't name-shaped.
fn collect_path_chain(cx: &LowerCtx, node: tree_sitter::Node<'_>) -> Option<Box<[Symbol]>> {
    let mut chain: Vec<Symbol> = Vec::new();
    fn walk(cx: &LowerCtx, node: tree_sitter::Node<'_>, out: &mut Vec<Symbol>) -> Option<()> {
        let kind = node.kind();
        if kind == "ident" || kind == "type_ident" {
            out.push(cx.symbols.intern(cx.text(node)));
            return Some(());
        }
        if kind == "static_expr" {
            // First named child is the head (static_expr or
            // type_ident), then optionally a `property` field.
            let mut cursor = node.walk();
            let head = node.named_children(&mut cursor).next()?;
            walk(cx, head, out)?;
            if let Some(prop) = node.child_by_field_name("property")
                && (prop.kind() == "ident" || prop.kind() == "string")
            {
                let text = if prop.kind() == "string" {
                    // `Foo::"variant"` — strip quotes.
                    string_literal_value(cx, prop)?
                } else {
                    cx.text(prop).to_string()
                };
                out.push(cx.symbols.intern(&text));
            }
            return Some(());
        }
        None
    }
    walk(cx, node, &mut chain)?;
    Some(chain.into_boxed_slice())
}

fn lit_to_annotation_arg(kind: LiteralKind) -> AnnotationArgKind {
    match kind {
        LiteralKind::Int(v) => AnnotationArgKind::Int(v),
        LiteralKind::Float(v) => AnnotationArgKind::Float(v),
        LiteralKind::Char(c) => AnnotationArgKind::Char(c),
        LiteralKind::Bool(b) => AnnotationArgKind::Bool(b),
        LiteralKind::Duration(v) => AnnotationArgKind::Duration(v),
        LiteralKind::Time(v) => AnnotationArgKind::Time(v),
        LiteralKind::Iso8601(v) => AnnotationArgKind::Iso8601(v),
    }
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

fn lower_generics(cx: &mut LowerCtx, node: Option<tree_sitter::Node<'_>>) -> Box<[Idx<Ident>]> {
    let Some(node) = node else {
        return Box::default();
    };
    let mut out: Vec<Idx<Ident>> = Vec::new();
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if c.kind() == "ident" {
            out.push(cx.alloc_ident(c));
        }
    }
    out.into_boxed_slice()
}

fn lower_fn_params(cx: &mut LowerCtx, node: Option<tree_sitter::Node<'_>>) -> Box<[Idx<FnParam>]> {
    let Some(node) = node else {
        return Box::default();
    };
    let mut out: Vec<Idx<FnParam>> = Vec::new();
    // P43.4 — salvage params hidden by ERROR recovery (e.g. typing
    // `fn foo(x: int, y: , z: bool)` puts the middle param in an ERROR;
    // the surrounding well-formed params still surface via the helper).
    for (c, _salvaged) in flatten_errors_named_children(node) {
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
    out.into_boxed_slice()
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
        // Type-body walk uses the salvage helper. When an
        // attr or method ends up half-typed (`x: int; foo.` in the
        // body), the surrounding well-formed members still surface.
        for (child, _salvaged) in flatten_errors_named_children(body) {
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
        attrs: attrs.into_boxed_slice(),
        methods: methods.into_boxed_slice(),
        doc: doc_text(cx, node),
        byte_range: node.byte_range(),
    })
}

fn lower_type_attr(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<TypeAttr> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_property_ident(name_node);
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
        // Enum-body walk via the salvage helper so a mid-edit
        // variant doesn't drop sibling variants.
        for (c, _salvaged) in flatten_errors_named_children(body) {
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
        fields: fields.into_boxed_slice(),
        doc: doc_text(cx, node),
        byte_range: node.byte_range(),
    })
}

fn lower_modvar(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<ModVarDecl> {
    let name_node = node.child_by_field_name("name")?;
    let name = cx.alloc_ident(name_node);
    let mut modifiers = lower_modifiers(cx, node.child_by_field_name("modifiers"));
    modifiers.annotations = lower_annotations(cx, node);
    // The grammar wraps the type in a `type_decorator` child;
    // `lower_type_ref` unwraps it back to the inner `type_ident`.
    let ty = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "type_decorator")
        .and_then(|n| lower_type_ref(cx, n));
    // The initializer (`= expr`) is invalid on a module-level `var`
    // (`parse_diagnostics` flags it) but still lowered so nested type /
    // resolve diagnostics fire on it.
    let init = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "initializer")
        .and_then(|i| {
            i.child_by_field_name("expr")
                .and_then(|e| lower_expr(cx, e))
        });
    Some(ModVarDecl {
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
        args: args.into_boxed_slice(),
        byte_range: node.byte_range(),
    })
}

fn lower_block(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Stmt>> {
    let block = lower_block_inline(cx, node)?;
    Some(cx.hir.stmts.alloc(Stmt::Block(block)))
}

/// Like [`lower_block`] but returns the [`BlockStmt`] directly without
/// allocating into the `stmts` arena. Body-bearing statements
/// (`If::then_branch`, `While::body`, …) embed it inline.
fn lower_block_inline(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<BlockStmt> {
    if node.kind() != "block" {
        return None;
    }
    let mut stmts = Vec::new();
    // Salvage helper so a statement-shaped child recovered
    // from an `ERROR` wrapper is still lowered. Expression-shaped
    // salvage (`(ERROR (member_expr …))`) is wrapped in `Stmt::Expr`.
    // Salvaged stmt ids are recorded so lints can skip them.
    for (c, salvaged) in flatten_errors_named_children(node) {
        let s_id = if let Some(s) = lower_stmt(cx, c) {
            s
        } else if salvaged {
            let Some(e) = lower_expr(cx, c) else { continue };
            cx.hir.stmts.alloc(Stmt::Expr(e))
        } else {
            continue;
        };
        if salvaged {
            cx.hir.salvaged_stmts.insert(s_id);
        }
        stmts.push(s_id);
    }
    salvage_incomplete_members_in_block(cx, node, &mut stmts);
    Some(BlockStmt {
        stmts: stmts.into_boxed_slice(),
        byte_range: node.byte_range(),
    })
}

/// Walk the block's CST subtree (stopping at nested `block` nodes)
/// for `member_expr` / `arrow_expr` with a missing `property`, lower
/// each receiver as a salvaged `Stmt::Expr`, and append it to the
/// stmt list. The grammar accepts `s.` / `c.sim.` mid-typing (no
/// ERROR-cascade) but lowers the incomplete member as
/// `Expr::Unsupported`, so the receiver wouldn't otherwise be typed.
fn salvage_incomplete_members_in_block(
    cx: &mut LowerCtx,
    block: tree_sitter::Node<'_>,
    stmts: &mut Vec<Idx<Stmt>>,
) {
    fn walk(cx: &mut LowerCtx, node: tree_sitter::Node<'_>, out: &mut Vec<Idx<Stmt>>) {
        let kind = node.kind();
        if (kind == "member_expr" || kind == "arrow_expr")
            && node.child_by_field_name("property").is_none()
            && let Some(receiver) = node.named_child(0)
            && let Some(e) = lower_expr(cx, receiver)
        {
            let s_id = cx.hir.stmts.alloc(Stmt::Expr(e));
            cx.hir.salvaged_stmts.insert(s_id);
            out.push(s_id);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            // Nested blocks salvage their own members.
            if child.kind() == "block" {
                continue;
            }
            walk(cx, child, out);
        }
    }
    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        if child.kind() == "block" {
            continue;
        }
        walk(cx, child, stmts);
    }
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
            // The type is either a direct `type_ident` child or wrapped
            // in a `type_decorator` (the canonical `var x: T`);
            // `lower_type_ref` handles both.
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
            // `_else_branch` is a hidden rule, so the `else_branch`
            // field tag sometimes doesn't propagate. Fall back to
            // scanning named children after the then_branch for the
            // next `block` / `if_stmt`.
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
            // P17.2 — `for_in_stmt` carries `sepBy2(",", for_in_param)`
            // (no field tag on the params), a field-tagged
            // `iterator: _expr`, and `block: block`. Walk named
            // children for the `for_in_param` nodes.
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
            // Iterable in `iterator: $._expr`, optional slice window in
            // `range: optional($.interval_expr)` (`[from..to]`).
            let iter_node = node.child_by_field_name("iterator")?;
            let iterator = lower_expr(cx, iter_node)?;
            // The null-ack `?` sits in one of two CST positions: a
            // direct `optional` child of `for_in_stmt` (Call/Ident/Paren
            // iterators), or the iterator's trailing `optional` last
            // named child (Member/Arrow/Offset absorb it as
            // `post_optional`). Clear the absorbed post so the iterator
            // keeps only its own `opt_chaining`.
            let nullable_iter = node
                .named_children(&mut node.walk())
                .find(|c| c.kind() == "optional")
                .or_else(|| {
                    iter_node
                        .named_children(&mut iter_node.walk())
                        .last()
                        .filter(|c| c.kind() == "optional")
                })
                .map(|c| c.byte_range());
            if nullable_iter.is_some() {
                match &mut cx.hir.exprs[iterator] {
                    Expr::Member(m) | Expr::Arrow(m) => m.post_optional = None,
                    Expr::Offset(o) => o.post_optional = None,
                    _ => {}
                }
            }
            let window = node
                .child_by_field_name("range")
                .and_then(|rng| lower_expr(cx, rng));
            let body = node
                .child_by_field_name("block")
                .and_then(|n| lower_block_inline(cx, n))?;
            Stmt::ForIn(ForInStmt {
                params: params.into_boxed_slice(),
                iterator,
                window,
                nullable_iter,
                body,
                byte_range: node.byte_range(),
            })
        }
        "return_stmt" => {
            let value = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|e| lower_expr(cx, e));
            Stmt::Return(ReturnStmt {
                value,
                byte_range: node.byte_range(),
            })
        }
        "break_stmt" => Stmt::Break(BreakStmt {
            byte_range: node.byte_range(),
        }),
        "continue_stmt" => Stmt::Continue(ContinueStmt {
            byte_range: node.byte_range(),
        }),
        "breakpoint_stmt" => Stmt::Breakpoint(BreakpointStmt {
            byte_range: node.byte_range(),
        }),
        "throw_stmt" => {
            let e = node
                .named_children(&mut node.walk())
                .next()
                .and_then(|x| lower_expr(cx, x))?;
            Stmt::Throw(ThrowStmt {
                value: e,
                byte_range: node.byte_range(),
            })
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

fn lower_expr(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Option<Idx<Expr>> {
    let kind = node.kind();
    // Comments are named nodes that surface in expression lists
    // (`[1, /* c */ 2]`, `Foo { /* c */ }`). A comment is never an
    // expression: bail with `None` so list-lowering callers skip it
    // instead of minting a phantom `Expr::Unsupported`.
    if matches!(kind, "line_comment" | "block_comment" | "doc_comment") {
        return None;
    }
    let expr = match kind {
        "ident" => {
            let id = cx.alloc_ident(node);
            Expr::Ident {
                name: id,
                byte_range: node.byte_range(),
            }
        }
        "null" => Expr::Null {
            byte_range: node.byte_range(),
        },
        "this" => Expr::This {
            byte_range: node.byte_range(),
        },
        "number" | "char" | "false" | "true" => {
            // Eager-parse every literal into its typed value plus an
            // optional `ParseIssue`. `char` is special: the grammar
            // nests `iso8601` inside the single quotes
            // (`'2024-01-01T00:00Z'`), so peek at the first named child
            // to choose between a char and an ISO-8601 literal.
            let raw = cx.text(node);
            let (lit_kind, parse_issue) = match kind {
                "number" => classify_and_parse_number(cx, node, raw, false),
                "char" => {
                    let iso_child = node
                        .named_children(&mut node.walk())
                        .find(|c| c.kind() == "iso8601");
                    match iso_child {
                        Some(iso) => parse_iso8601(cx.text(iso)),
                        None => parse_char(raw),
                    }
                }
                "false" => (LiteralKind::Bool(false), None),
                "true" => (LiteralKind::Bool(true), None),
                _ => unreachable!(),
            };
            Expr::Literal(LiteralExpr {
                kind: lit_kind,
                parse_issue,
                byte_range: node.byte_range(),
            })
        }
        "string" => {
            // P17.5 — capture text fragments and `${expr}`
            // interpolations in source order. Non-template strings are
            // a single `Lit`; templates alternate `Lit` / `Interp`.
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
                parts: parts.into_boxed_slice(),
                byte_range: node.byte_range(),
            })
        }
        "tuple_expr" => {
            let parts = lower_expr_list(cx, node);
            Expr::Tuple(parts.into_boxed_slice(), node.byte_range())
        }
        "array_expr" => {
            let parts = lower_expr_list(cx, node);
            Expr::Array(parts.into_boxed_slice(), node.byte_range())
        }
        "paren_expr" => {
            let inner = node
                .child_by_field_name("expr")
                .and_then(|e| lower_expr(cx, e))?;
            Expr::Paren(inner, node.byte_range())
        }
        "member_expr" => {
            // The grammar accepts `s.` mid-typing. Lower the
            // no-property case as Unsupported (hard diagnostic comes
            // from `core::diagnostics`); the receiver is salvaged by
            // `salvage_incomplete_members_in_block`.
            let Some(prop) = node.child_by_field_name("property") else {
                let id = cx.hir.exprs.alloc(Expr::Unsupported {
                    kind: "member_expr_missing_property",
                    byte_range: node.byte_range(),
                });
                return Some(id);
            };
            let receiver =
                first_named_child_excluding(node, prop.id()).and_then(|n| lower_expr(cx, n))?;
            let property = cx.alloc_property_name(prop);
            let (opt_chaining, post_optional) = cst::optional_flags_around(node, prop.id());
            Expr::Member(MemberExpr {
                receiver,
                property,
                opt_chaining,
                post_optional,
                byte_range: node.byte_range(),
            })
        }
        "arrow_expr" => {
            // See `member_expr` arm.
            let Some(prop) = node.child_by_field_name("property") else {
                let id = cx.hir.exprs.alloc(Expr::Unsupported {
                    kind: "arrow_expr_missing_property",
                    byte_range: node.byte_range(),
                });
                return Some(id);
            };
            let receiver =
                first_named_child_excluding(node, prop.id()).and_then(|n| lower_expr(cx, n))?;
            let property = cx.alloc_property_name(prop);
            let (opt_chaining, post_optional) = cst::optional_flags_around(node, prop.id());
            Expr::Arrow(MemberExpr {
                receiver,
                property,
                opt_chaining,
                post_optional,
                byte_range: node.byte_range(),
            })
        }
        "static_expr" => {
            // P15.8 — a chained `module::Type::name` head is itself a
            // `static_expr`, so it can't fit `StaticExpr { ty, property
            // }`; lower it as `Expr::QualifiedStatic` with flat idents.
            // The grammar accepts `Foo::` (no property) mid-typing;
            // lower that case as Unsupported (hard diagnostic from
            // `core::diagnostics::static_property_diagnostics`).
            let Some(prop) = node.child_by_field_name("property") else {
                let id = cx.hir.exprs.alloc(Expr::Unsupported {
                    kind: "static_expr_missing_property",
                    byte_range: node.byte_range(),
                });
                return Some(id);
            };
            let head = first_named_child_excluding(node, prop.id())?;
            if head.kind() == "static_expr" {
                let mut chain = Vec::new();
                if !collect_static_chain_idents(cx, node, &mut chain) {
                    return None;
                }
                Expr::QualifiedStatic {
                    chain: chain.into_boxed_slice(),
                    byte_range: node.byte_range(),
                }
            } else {
                let ty = lower_type_ref(cx, head)?;
                // Property is an `ident` (`Foo::a`) or quoted `string`
                // (`Foo::"a"`); `PropertyName` preserves the form.
                let property = cx.alloc_property_name(prop);
                Expr::Static(StaticExpr {
                    ty,
                    property,
                    byte_range: node.byte_range(),
                })
            }
        }
        "offset_expr" => {
            // `recv[index]` — two `_expr` children, plus `optional`
            // tokens for the null-safe forms `a?[i]` / `a[i]?`. Classify
            // each `optional` by its position relative to receiver / index.
            let mut cursor = node.walk();
            let mut recv: Option<Idx<Expr>> = None;
            let mut idx: Option<Idx<Expr>> = None;
            let mut pre_optional: Option<Span> = None;
            let mut post_optional: Option<Span> = None;
            for c in node.named_children(&mut cursor) {
                match c.kind() {
                    "optional" => {
                        if recv.is_some() && idx.is_some() {
                            post_optional = Some(c.byte_range());
                        } else if recv.is_some() {
                            // Before the index — the `a?[i]` shape.
                            pre_optional = Some(c.byte_range());
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
                args: args.into_boxed_slice(),
                byte_range: node.byte_range(),
            })
        }
        "binary_expr" => {
            // P6.5: `is` / `as` carry a `type_ident` on the right;
            // emit dedicated HIR variants rather than Binary.
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
            let op = unary_op_for(operator_text(cx, node));
            // Fold `-<number>` into one negated literal so the
            // i64-boundary asymmetry (magnitude up to `2^63` is valid
            // for `i64::MIN`, only `2^63-1` for a positive result) is
            // applied here — e.g. `-9223372036854775808` must not
            // saturate the magnitude before negating. Same for
            // `time` / `duration` / `float` suffixes.
            let operand_node = node.named_children(&mut node.walk()).next();
            if matches!(op, UnaryOp::Neg)
                && let Some(child) = operand_node
                && child.kind() == "number"
            {
                let raw = cx.text(child);
                let (kind, parse_issue) = classify_and_parse_number(cx, child, raw, true);
                Expr::Literal(LiteralExpr {
                    kind,
                    parse_issue,
                    byte_range: node.byte_range(),
                })
            } else {
                let operand = operand_node.and_then(|n| lower_expr(cx, n))?;
                Expr::Unary(UnaryExpr {
                    op,
                    operand,
                    byte_range: node.byte_range(),
                })
            }
        }
        "lambda_expr" => {
            let params = lower_fn_params(cx, node.child_by_field_name("params"));
            let return_type = node
                .child_by_field_name("return_type")
                .and_then(|n| lower_type_ref(cx, n));
            let body = node
                .child_by_field_name("body")
                .and_then(|n| lower_block_inline(cx, n))?;
            Expr::Lambda(LambdaExpr {
                params,
                return_type,
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
            // The two body productions are disjoint and map to distinct
            // HIR variants: `object_initializers` → `PositionalObject`,
            // `object_fields` → `Object`. Consumers read the
            // construction form off the variant.
            let ty = node
                .child_by_field_name("type")
                .and_then(|n| lower_type_ref(cx, n));
            let mut positional_fields: Option<Vec<Idx<Expr>>> = None;
            let mut named_fields: Option<Vec<ObjectField>> = None;
            let mut walk = node.walk();
            for child in node.named_children(&mut walk) {
                match child.kind() {
                    "object_initializers" => {
                        // Positional `Foo { a, b, c }` salvage.
                        let mut fields = Vec::new();
                        for (value_node, _salvaged) in flatten_errors_named_children(child) {
                            if let Some(value) = lower_expr(cx, value_node) {
                                fields.push(value);
                            }
                        }
                        positional_fields = Some(fields);
                    }
                    "object_fields" => {
                        // Named `Foo { a: x, b: y }` salvage.
                        let mut fields = Vec::new();
                        for (of, _salvaged) in flatten_errors_named_children(child) {
                            if of.kind() != "object_field" {
                                continue;
                            }
                            // `name` is `_expr` — a bare ident / quoted
                            // string for a classic field, or an
                            // arbitrary key expr for a `Map`
                            // (`Map { Level::Low: 0 }`). The
                            // field-vs-key interpretation is made
                            // downstream by the head type.
                            let name = of
                                .child_by_field_name("name")
                                .and_then(|n| lower_expr(cx, n));
                            let value = of
                                .child_by_field_name("value")
                                .and_then(|v| lower_expr(cx, v));
                            if let (Some(name), Some(value)) = (name, value) {
                                fields.push(ObjectField {
                                    name,
                                    value,
                                    byte_range: of.byte_range(),
                                });
                            }
                        }
                        named_fields = Some(fields);
                    }
                    _ => {}
                }
            }
            match (ty, named_fields, positional_fields) {
                // Named body wins; neither body falls back to an empty
                // positional shape (empty `{}` parses as
                // `object_initializers`).
                (Some(ty), Some(fields), _) => Expr::Object(ObjectExpr {
                    ty,
                    fields: fields.into_boxed_slice(),
                    byte_range: node.byte_range(),
                }),
                (Some(ty), None, positional) => Expr::PositionalObject(PositionalObjectExpr {
                    ty,
                    fields: positional.unwrap_or_default().into_boxed_slice(),
                    byte_range: node.byte_range(),
                }),
                _ => Expr::Unsupported {
                    kind: "anonymous-object",
                    byte_range: node.byte_range(),
                },
            }
        }
        // `from..to` (and `from..` / `..to`) plus the
        // math-style `]from..to]` / `[from..to[` interval flatten into
        // one HIR shape; bracket inclusivity doesn't affect typing.
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

/// Yield named children of `node`, flattening one level into any
/// `ERROR` child so its grandchildren appear in place. Each item is
/// `(child, salvaged_from_error)`: `true` when the child came from an
/// `ERROR` wrapper (e.g. `if (c.sim.)` → `(block (ERROR (member_expr
/// ...)))`). Depth-bounded — an `ERROR` nested inside an `ERROR` stays
/// opaque.
fn flatten_errors_named_children<'a>(
    node: tree_sitter::Node<'a>,
) -> Vec<(tree_sitter::Node<'a>, bool)> {
    // Collect into a Vec — tree_sitter cursors are hostile to nested
    // iteration. Child lists here are short.
    let mut out: Vec<(tree_sitter::Node<'a>, bool)> = Vec::new();
    let mut cur1 = node.walk();
    for c in node.named_children(&mut cur1) {
        if c.kind() == "ERROR" {
            let mut cur2 = c.walk();
            for g in c.named_children(&mut cur2) {
                if g.kind() == "ERROR" {
                    // Bounded descent: ERROR-in-ERROR stays opaque.
                    continue;
                }
                out.push((g, true));
            }
        } else {
            out.push((c, false));
        }
    }
    out
}

/// Classify a `number` CST node by its typed suffix and parse its
/// body in one pass. Grammar shape: `(number (number_suffixed
/// (number_int | number_decimal | number_scientific)
/// (number_suffix)?))`.
///
/// Suffix dispatch:
/// - `time` → [`LiteralKind::Time`] (i64 µs).
/// - `duration` / unit suffixes (`y`, `d`, `h`, `m`, `s`, `ms`, `us`,
///   `ns`, … and long forms) → [`LiteralKind::Duration`], scaled to µs.
/// - bare `f` / `_f` → [`LiteralKind::Float`].
/// - no suffix: `.` or `e`/`E` exponent → [`LiteralKind::Float`],
///   otherwise [`LiteralKind::Int`].
fn classify_and_parse_number(
    cx: &LowerCtx,
    node: tree_sitter::Node<'_>,
    raw: &str,
    negate: bool,
) -> (LiteralKind, Option<ParseIssue>) {
    let (body_text, suffix_text) = extract_number_parts(cx, node);
    let suffix = suffix_text.as_deref().map(|s| s.trim_start_matches('_'));
    let body = body_text.as_deref().unwrap_or(raw);
    let to_signed = if negate {
        magnitude_to_i64_negated
    } else {
        magnitude_to_i64_positive
    };
    match suffix {
        Some("time") => {
            let (m, issue) = parse_integer_magnitude_sat(body);
            let (n, issue) = to_signed(m, issue);
            (LiteralKind::Time(n), issue)
        }
        Some(s) if matches!(s, "us" | "ms" | "s" | "min" | "hour" | "day") => {
            let (m, issue) = parse_integer_magnitude_sat(body);
            let (n, mut issue) = to_signed(m, issue);
            let us = match duration_to_us(n, s) {
                Some(us) => us,
                None => {
                    issue.get_or_insert(ParseIssue::Overflow);
                    if negate { i64::MIN } else { i64::MAX }
                }
            };
            (LiteralKind::Duration(us), issue)
        }
        Some("f") | Some("F") => {
            let (f, issue) = parse_float_sat(body);
            (LiteralKind::Float(if negate { -f } else { f }), issue)
        }
        suffix => {
            if looks_like_float(body) {
                let (f, issue) = parse_float_sat(body);
                let issue = match suffix {
                    Some(_) => issue.or(Some(ParseIssue::Suffix)),
                    None => issue,
                };
                (LiteralKind::Float(if negate { -f } else { f }), issue)
            } else {
                let (m, issue) = parse_integer_magnitude_sat(body);
                let (n, issue) = to_signed(m, issue);
                let issue = match suffix {
                    Some(_) => issue.or(Some(ParseIssue::Suffix)),
                    None => issue,
                };
                (LiteralKind::Int(n), issue)
            }
        }
    }
}

/// Recover a `number` node's body text (digits without suffix) and
/// suffix text. Returns `(None, None)` on a structure mismatch —
/// caller falls back to the full `raw` token.
fn extract_number_parts(
    cx: &LowerCtx,
    node: tree_sitter::Node<'_>,
) -> (Option<String>, Option<String>) {
    let mut body: Option<String> = None;
    let mut suffix: Option<String> = None;
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() == "number_suffixed" {
            let mut sc = child.walk();
            for sub in child.named_children(&mut sc) {
                match sub.kind() {
                    "number_int" | "number_decimal" | "number_scientific" => {
                        body = Some(cx.text(sub).to_string());
                    }
                    "number_suffix" => {
                        suffix = Some(cx.text(sub).to_string());
                    }
                    _ => {}
                }
            }
        } else if matches!(
            child.kind(),
            "number_int" | "number_decimal" | "number_scientific"
        ) {
            body = Some(cx.text(child).to_string());
        }
    }
    (body, suffix)
}

/// `true` if a numeric body reads as a float — has a decimal point or
/// a scientific exponent.
fn looks_like_float(text: &str) -> bool {
    if text.contains('.') {
        return true;
    }
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if (b == b'e' || b == b'E')
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && let Some(&next) = bytes.get(i + 1)
            && (next == b'+' || next == b'-' || next.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Saturating decimal integer magnitude parse. The grammar's
/// `number_int` is `[0-9][0-9_]*` (base-10 with optional `_`). Returns
/// the absolute magnitude as `u64`; the `i64` boundary (and its
/// positive/negative asymmetry) is applied by
/// [`magnitude_to_i64_positive`] / [`magnitude_to_i64_negated`].
fn parse_integer_magnitude_sat(s: &str) -> (u64, Option<ParseIssue>) {
    let mut acc: u64 = 0;
    let mut overflow = false;
    for &b in s.as_bytes() {
        if b == b'_' {
            continue;
        }
        if !b.is_ascii_digit() {
            // Grammar guarantees digits; surface defensively.
            return (u64::MAX, Some(ParseIssue::Overflow));
        }
        if overflow {
            continue;
        }
        let d = (b - b'0') as u64;
        match acc.checked_mul(10).and_then(|v| v.checked_add(d)) {
            Some(v) => acc = v,
            None => {
                overflow = true;
                acc = u64::MAX;
            }
        }
    }
    (acc, overflow.then_some(ParseIssue::Overflow))
}

/// Convert a magnitude to a positive `i64`, saturating at `i64::MAX`.
/// Magnitudes greater than `i64::MAX` flag [`ParseIssue::Overflow`].
fn magnitude_to_i64_positive(m: u64, mut issue: Option<ParseIssue>) -> (i64, Option<ParseIssue>) {
    if m > i64::MAX as u64 {
        issue.get_or_insert(ParseIssue::Overflow);
        (i64::MAX, issue)
    } else {
        (m as i64, issue)
    }
}

/// Convert a magnitude to its negation as `i64`. `i64::MIN` has
/// magnitude `2^63`, exactly representable as the negated value (hence
/// the split positive / negated converters). Magnitudes `> 2^63` flag
/// [`ParseIssue::Overflow`] and saturate at `i64::MIN`.
fn magnitude_to_i64_negated(m: u64, mut issue: Option<ParseIssue>) -> (i64, Option<ParseIssue>) {
    const I64_MIN_MAG: u64 = i64::MIN.unsigned_abs();
    match m.cmp(&I64_MIN_MAG) {
        std::cmp::Ordering::Less => ((m as i64).wrapping_neg(), issue),
        std::cmp::Ordering::Equal => (i64::MIN, issue),
        std::cmp::Ordering::Greater => {
            issue.get_or_insert(ParseIssue::Overflow);
            (i64::MIN, issue)
        }
    }
}

/// f64 parse. Strips GreyCat's underscore digit separators then
/// delegates to Rust's correctly-rounded `str::parse::<f64>`. Only
/// [`ParseIssue::Overflow`] surfaces, and only when the parsed value
/// is `±∞`; [`ParseIssue::PrecisionLoss`] is defined but not emitted.
fn parse_float_sat(s: &str) -> (f64, Option<ParseIssue>) {
    // GreyCat allows `_` as a digit-grouping marker (`1_000.5_e+1_0`);
    // Rust's parser rejects them. Strip only when present so the
    // common no-underscore case skips the allocation.
    let cleaned: std::borrow::Cow<'_, str> = if s.contains('_') {
        std::borrow::Cow::Owned(s.chars().filter(|c| *c != '_').collect())
    } else {
        std::borrow::Cow::Borrowed(s)
    };
    let value = match cleaned.parse::<f64>() {
        Ok(v) => v,
        // Grammar already validated the shape; defensive return.
        Err(_) => return (0.0, Some(ParseIssue::Overflow)),
    };
    if value.is_infinite() {
        return (value, Some(ParseIssue::Overflow));
    }
    (value, None)
}

fn parse_char(raw: &str) -> (LiteralKind, Option<ParseIssue>) {
    // Char tokens are single-quoted. Strip exactly the delimiter
    // quotes — `trim_matches` would also eat the escaped `'` in `'\''`
    // — then decode the (possibly-escaped) inner content.
    let inner = raw
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(raw);
    let decoded: Option<char> = if let Some(esc) = inner.strip_prefix('\\') {
        match esc {
            "n" => Some('\n'),
            "r" => Some('\r'),
            "t" => Some('\t'),
            "\\" => Some('\\'),
            "'" => Some('\''),
            "\"" => Some('"'),
            "0" => Some('\0'),
            _ => None,
        }
    } else {
        let mut chars = inner.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    };
    match decoded {
        Some(c) => (LiteralKind::Char(c), None),
        None => (LiteralKind::Char('\0'), Some(ParseIssue::Malformed)),
    }
}

/// Parse an ISO-8601 literal to µs since the Unix epoch.
///
/// Accepted shapes (the date prefix is mandatory; everything after
/// is optional and parsed greedily):
///   - `YYYY-MM-DD`
///   - `YYYY-MM-DD[T| ]HH:MM:SS`
///   - `…HH:MM:SS.f{1..9}`  (fractional seconds, truncated to µs)
///   - `…Z`                  (UTC marker, no-op)
///   - `…±HH:MM` or `…±HHMM` (timezone offset; subtracted to get UTC)
///
/// No allocation — byte walker only. Returns
/// [`ParseIssue::Malformed`] on any structural mismatch (out-of-range
/// component, unrecognised trailing bytes, …) and
/// [`ParseIssue::Overflow`] when the resulting µs count doesn't fit
/// i64 (year well outside the ~±292,277-year representable window).
fn parse_iso8601(raw: &str) -> (LiteralKind, Option<ParseIssue>) {
    let bytes = raw.as_bytes();
    let mut i = 0;

    let malformed = || (LiteralKind::Iso8601(0), Some(ParseIssue::Malformed));

    let Some(year) = read_digits(bytes, &mut i, 4) else {
        return malformed();
    };
    if bytes.get(i) != Some(&b'-') {
        return malformed();
    }
    i += 1;
    let Some(month) = read_digits(bytes, &mut i, 2) else {
        return malformed();
    };
    if !(1..=12).contains(&month) {
        return malformed();
    }
    if bytes.get(i) != Some(&b'-') {
        return malformed();
    }
    i += 1;
    let Some(day) = read_digits(bytes, &mut i, 2) else {
        return malformed();
    };
    if day == 0 || day > days_in_month(year as i32, month as u32) as u64 {
        return malformed();
    }

    let mut hour = 0u64;
    let mut minute = 0u64;
    let mut second = 0u64;
    let mut frac_us: i64 = 0;
    let mut tz_offset_minutes: i32 = 0;

    if matches!(bytes.get(i), Some(b'T') | Some(b' ')) {
        i += 1;
        let Some(h) = read_digits(bytes, &mut i, 2) else {
            return malformed();
        };
        hour = h;
        if hour > 23 || bytes.get(i) != Some(&b':') {
            return malformed();
        }
        i += 1;
        let Some(m) = read_digits(bytes, &mut i, 2) else {
            return malformed();
        };
        minute = m;
        if minute > 59 || bytes.get(i) != Some(&b':') {
            return malformed();
        }
        i += 1;
        let Some(s) = read_digits(bytes, &mut i, 2) else {
            return malformed();
        };
        second = s;
        // Leap second (`60`) is tolerated by the runtime; cap there.
        if second > 60 {
            return malformed();
        }

        // Fractional seconds, capped at 6 digits (µs). Extra digits
        // are valid ISO-8601 but quietly truncated.
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            let mut frac = 0i64;
            let mut k = 0;
            while k < 6 && bytes.get(i).copied().is_some_and(|b| b.is_ascii_digit()) {
                frac = frac * 10 + (bytes[i] - b'0') as i64;
                i += 1;
                k += 1;
            }
            for _ in k..6 {
                frac *= 10;
            }
            while bytes.get(i).copied().is_some_and(|b| b.is_ascii_digit()) {
                i += 1;
            }
            frac_us = frac;
        }

        // Timezone marker.
        match bytes.get(i).copied() {
            Some(b'Z') => i += 1,
            Some(sign @ (b'+' | b'-')) => {
                i += 1;
                let Some(tz_h) = read_digits(bytes, &mut i, 2) else {
                    return malformed();
                };
                if tz_h > 14 {
                    return malformed();
                }
                let tz_m = if bytes.get(i) == Some(&b':') {
                    i += 1;
                    let Some(m) = read_digits(bytes, &mut i, 2) else {
                        return malformed();
                    };
                    m
                } else if bytes.get(i).copied().is_some_and(|b| b.is_ascii_digit()) {
                    // `±HHMM` form: two more digits directly.
                    let Some(m) = read_digits(bytes, &mut i, 2) else {
                        return malformed();
                    };
                    m
                } else {
                    0
                };
                if tz_m > 59 {
                    return malformed();
                }
                let total = (tz_h as i32) * 60 + tz_m as i32;
                tz_offset_minutes = if sign == b'-' { -total } else { total };
            }
            None => {}
            _ => return malformed(),
        }
    }

    // Reject trailing garbage.
    if i != bytes.len() {
        return malformed();
    }

    let days = days_from_civil(year as i32, month as u32, day as u32);
    let Some(secs) = days
        .checked_mul(86_400)
        .and_then(|v| v.checked_add(hour as i64 * 3600))
        .and_then(|v| v.checked_add(minute as i64 * 60))
        .and_then(|v| v.checked_add(second as i64))
    else {
        return (LiteralKind::Iso8601(0), Some(ParseIssue::Overflow));
    };
    let Some(mut us) = secs
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac_us))
    else {
        return (LiteralKind::Iso8601(0), Some(ParseIssue::Overflow));
    };
    // Local time → UTC: subtract the offset.
    let tz_us = tz_offset_minutes as i64 * 60 * 1_000_000;
    us = match us.checked_sub(tz_us) {
        Some(v) => v,
        None => return (LiteralKind::Iso8601(0), Some(ParseIssue::Overflow)),
    };
    (LiteralKind::Iso8601(us), None)
}

/// Read exactly `n` ascii digits starting at `*i`, advance `*i` past
/// them, and return the parsed integer. Returns `None` if the bytes
/// at `[*i..*i+n]` aren't all digits (or the slice runs short).
fn read_digits(bytes: &[u8], i: &mut usize, n: usize) -> Option<u64> {
    if *i + n > bytes.len() {
        return None;
    }
    let mut v: u64 = 0;
    for k in 0..n {
        let b = bytes[*i + k];
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u64;
    }
    *i += n;
    Some(v)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's days-from-civil — days from `1970-01-01` to
/// `(year, month, day)`. Negative for dates before the epoch. Valid
/// for the full proleptic Gregorian range that fits an i64 day count.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = (year as i64) - i64::from(month <= 2);
    let m = month as i64;
    let d = day as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy;
    era * 146_097 + doe - 719_468
}

/// Convert `value <suffix>` to microseconds. `None` on overflow or unknown suffix.
fn duration_to_us(value: i64, suffix: &str) -> Option<i64> {
    const MS: i64 = 1_000;
    const SEC: i64 = 1_000_000;
    const MIN: i64 = 60 * SEC;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    match suffix {
        "us" => Some(value),
        "ms" => value.checked_mul(MS),
        "s" => value.checked_mul(SEC),
        "min" => value.checked_mul(MIN),
        "hour" => value.checked_mul(HOUR),
        "day" => value.checked_mul(DAY),
        _ => None,
    }
}

fn lower_expr_list(cx: &mut LowerCtx, node: tree_sitter::Node<'_>) -> Vec<Idx<Expr>> {
    // P43.4 — covers `tuple_expr`, `array_expr`, `call_expr` args, and
    // `object_initializers`. The salvage helper lets recognizable inner
    // shapes land in the list even when a sibling arg is mid-edit
    // (`foo(bar., baz)` wraps `bar.` in an ERROR).
    let mut out = Vec::new();
    for (c, _salvaged) in flatten_errors_named_children(node) {
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

// P15.8
/// Alloc each segment of a chained `static_expr` into the idents
/// arena, pushing the `Idx<Ident>` into `out`. `false` on a malformed
/// chain. `runtime::Identity::create` → `[runtime, Identity, create]`.
/// The leftmost segment is a `type_ident`; the rest come from each
/// `static_expr.property`.
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

fn operator_text<'src, 'symbols>(
    cx: &LowerCtx<'src, 'symbols>,
    node: tree_sitter::Node<'_>,
) -> &'src str {
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
        "+" => UnaryOp::Pos,
        "!" => UnaryOp::Not,
        "~" => UnaryOp::BitNot,
        "++" => UnaryOp::Inc,
        "--" => UnaryOp::Dec,
        "!!" => UnaryOp::NonNullAssert,
        "*" => UnaryOp::Deref,
        _ => UnaryOp::Not,
    }
}

/// Intern a tree-sitter kind string into `&'static` via a leak — the
/// set of node kinds is bounded (~70), so the leak is bounded too.
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
    // Ids live in `type_ident` (field `name` + optional `params`);
    // `attr_type` / `type_decorator` embed one.
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
    // Bucket named children by kind: `ident` siblings before the name
    // → module-qualifier segments (leftmost-first); `type_ident`
    // siblings → generic params. The grammar emits one `params:`-tagged
    // child per arg (`Map<K, V>` → two), so walk all named children
    // rather than `child_by_field_name("params")` (returns only the first).
    let mut qualifier: Vec<Idx<Ident>> = Vec::new();
    let mut params: Vec<Idx<TypeRef>> = Vec::new();
    let mut cursor = inner.walk();
    for c in inner.named_children(&mut cursor) {
        if c.id() == name_node.id() {
            continue;
        }
        match c.kind() {
            "ident" => {
                qualifier.push(cx.alloc_ident(c));
            }
            "type_ident" => {
                if let Some(tp) = lower_type_ref(cx, c) {
                    params.push(tp);
                }
            }
            _ => {}
        }
    }
    let optional = inner
        .named_children(&mut inner.walk())
        .any(|c| c.kind() == "optional");
    // Anonymous `typeof` keyword child at the head of `type_ident`
    // (the parser prefers this over the `fn_param` slot). Carried
    // through to type lowering as `TypeKind::TypeOf(inner)`.
    let typeof_marker = inner
        .children(&mut inner.walk())
        .any(|c| c.kind() == "typeof");
    Some(cx.hir.type_refs.alloc(TypeRef {
        qualifier: qualifier.into_boxed_slice(),
        name,
        params: params.into_boxed_slice(),
        optional,
        typeof_marker,
        // P18.1 — the `type_ident`'s own range, not the wrapper's
        // (`attr_type` / `type_decorator` include leading `:` /
        // annotation tokens); matches the parity oracle's span.
        byte_range: range_of(inner),
    }))
}

fn range_of(node: tree_sitter::Node<'_>) -> Range<usize> {
    node.byte_range()
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_syntax::parse;

    /// Locate the first descendant whose kind matches `kind` (DFS).
    fn find_first<'tree>(
        node: tree_sitter::Node<'tree>,
        kind: &str,
    ) -> Option<tree_sitter::Node<'tree>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cursor = node.walk();
        for c in node.named_children(&mut cursor) {
            if let Some(found) = find_first(c, kind) {
                return Some(found);
            }
        }
        None
    }

    // P43.1
    /// Direct children of a non-`ERROR` node come through unchanged
    /// with `salvaged = false`.
    #[test]
    fn flatten_errors_passes_through_direct_children() {
        let src = "fn f() { var x = 1; var y = 2; }\n";
        let tree = parse(src);
        let block = find_first(tree.root_node(), "block").expect("block node");
        let items = flatten_errors_named_children(block);
        assert_eq!(items.len(), 2, "two stmts expected");
        assert!(items.iter().all(|(_, salvaged)| !*salvaged));
        assert_eq!(items[0].0.kind(), "var_decl");
        assert_eq!(items[1].0.kind(), "var_decl");
    }

    // P43.1
    /// `if (c.sim.)` — tree-sitter wraps the salvaged `member_expr` in
    /// `ERROR`; the helper flattens it back with `salvaged = true`.
    #[test]
    fn flatten_errors_descends_one_level_into_error() {
        let src = "type C { x: int; }\nfn test(c: C) {\n    if (c.x.)\n}\n";
        let tree = parse(src);
        let block = {
            // The fn_decl's block has the ERROR-wrapped member_expr.
            let root = tree.root_node();
            let mut cursor = root.walk();
            let fn_decl = root
                .named_children(&mut cursor)
                .find(|c| c.kind() == "fn_decl")
                .expect("fn_decl");
            fn_decl.child_by_field_name("body").expect("fn body")
        };
        let items = flatten_errors_named_children(block);
        // Should see the salvaged member_expr (line_comments may also
        // appear; filter by kind we care about).
        let member_exprs: Vec<_> = items
            .iter()
            .filter(|(n, _)| n.kind() == "member_expr")
            .collect();
        assert_eq!(member_exprs.len(), 1, "salvaged member_expr surfaces");
        assert!(member_exprs[0].1, "salvaged flag is set");
    }

    // P43.1
    /// Invariant: yielded items are never themselves `ERROR`. Checked
    /// against a few error-prone shapes (incomplete expr, trailing
    /// operator).
    #[test]
    fn flatten_errors_never_yields_error_kind() {
        let inputs = [
            "type C { x: int; }\nfn test(c: C) {\n    if (c.x.)\n}\n",
            "fn f() {\n    var x = ;\n}\n",
            "fn f() {\n    foo(\n}\n",
        ];
        for src in inputs {
            let tree = parse(src);
            let block = find_first(tree.root_node(), "block").expect("a block");
            for (n, _) in flatten_errors_named_children(block) {
                assert_ne!(
                    n.kind(),
                    "ERROR",
                    "yielded item must not be ERROR (src: {src:?})"
                );
            }
        }
    }

    /// `parse_char` decodes plain chars and every escape, including the
    /// escaped single quote `'\''` (runtime-verified valid, prints `'`).
    #[test]
    fn parse_char_decodes_escapes() {
        let ok = |raw: &str, want: char| {
            assert_eq!(parse_char(raw), (LiteralKind::Char(want), None), "{raw}");
        };
        ok("'a'", 'a');
        ok("'\\''", '\''); // escaped single quote — was over-stripped
        ok("'\\\\'", '\\');
        ok("'\\n'", '\n');
        ok("'\\r'", '\r');
        ok("'\\t'", '\t');
        ok("'\\\"'", '"');
        ok("'\\0'", '\0');
        // Unknown escape and empty char stay malformed.
        assert_eq!(parse_char("'\\z'").1, Some(ParseIssue::Malformed));
        assert_eq!(parse_char("''").1, Some(ParseIssue::Malformed));
    }
}
