//! CST → Doc lowering.
//!
//! Walks the tree-sitter CST in named-structure order and produces a
//! `Doc` tree the renderer can lay out under a width budget.
//!
//! The lowering carries no `last_byte` / "what was last emitted" state —
//! every spacing decision is made from the *structure* (kind of node,
//! position in its parent's child list) plus a **gap probe** into the
//! source between two known byte offsets when blank-line preservation
//! requires it. Each `lower_*` function returns a `Doc` and is local
//! enough to be reasoned about in isolation.
//!
//! A "verbatim" fallback at the bottom of `lower_node` emits the node's
//! raw source text for kinds the lowering doesn't yet handle. The
//! fallback is what makes incremental progress possible — every chunk
//! lights up specific constructs and the rest fall through to verbatim.

use crate::doc::Doc;
use crate::trivia::{GapItem, scan_gap};
use greycat_analyzer_syntax::tree_sitter::Node;

/// Lowering context — owns the source, threads through every visitor.
pub struct Cx<'a> {
    source: &'a str,
}

impl<'a> Cx<'a> {
    pub fn new(source: &'a str) -> Self {
        Cx { source }
    }

    pub fn text(&self, node: Node<'_>) -> &'a str {
        &self.source[node.byte_range()]
    }

    /// Source byte gap between two non-overlapping nodes — `[a.end, b.start)`.
    fn gap(&self, a: Node<'_>, b: Node<'_>) -> std::ops::Range<usize> {
        a.end_byte()..b.start_byte()
    }

    /// Number of newlines between two siblings in the source.
    fn newlines_between(&self, a: Node<'_>, b: Node<'_>) -> u32 {
        crate::trivia::newline_count(self.source, self.gap(a, b))
    }
}

/// Top-level entry — lower the `module` root node into a Doc.
pub fn lower_module<'a>(cx: &Cx<'a>, root: Node<'a>) -> Doc {
    let mut parts: Vec<Doc> = Vec::new();
    let mut prev: Option<Node<'a>> = None;
    let mut walker = root.walk();
    for child in root.named_children(&mut walker) {
        if let Some(p) = prev {
            // Source-driven blank-line preservation: count newlines in
            // the gap and emit (count) hardlines so the user's vertical
            // spacing survives, capped at 4 (one terminator + 3 blanks).
            let mut nls = cx.newlines_between(p, child).min(4);
            // But: doc-comments live inside their host decl (in the
            // `doc` field), so the gap between a `line_comment` extra
            // and the next decl shouldn't grow if the user happened to
            // separate them — actually the legacy treats a blank
            // before a doc-attached decl specially. We replicate that
            // in the per-decl visitor for the doc-vs-rest gap; module
            // level just preserves the raw count.
            if nls == 0 {
                // No newline between two top-level constructs in source
                // (e.g. one-line file with two decls — unusual). Force
                // at least one to keep the output legal.
                nls = 1;
            }
            for _ in 0..nls {
                parts.push(Doc::hardline());
            }
        }
        parts.push(lower_node(cx, child));
        prev = Some(child);
    }
    Doc::concat(parts)
}

/// Dispatch on `node.kind()`. Unknown kinds fall through to `verbatim`.
fn lower_node<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    match node.kind() {
        // Decls
        "mod_pragma" => lower_mod_pragma(cx, node),
        "fn_decl" => lower_fn_decl(cx, node),
        "type_decl" => lower_type_decl(cx, node),
        "enum_decl" => lower_enum_decl(cx, node),
        "modvar" => lower_modvar(cx, node),
        // Bodies
        "type_body" => lower_type_body(cx, node),
        "enum_body" => lower_enum_body(cx, node),
        "block" => lower_block(cx, node),
        // Decl-internals
        "type_attr" => lower_type_attr(cx, node),
        "type_method" => lower_type_method(cx, node),
        "enum_field" => lower_enum_field(cx, node),
        "fn_params" => lower_fn_params(cx, node),
        "fn_param" => lower_fn_param(cx, node),
        "type_params" => lower_type_params(cx, node),
        "type_decorator" | "attr_type" => lower_type_decorator(cx, node),
        "attr_init" | "initializer" => lower_initializer(cx, node),
        "modifiers" => lower_modifiers(cx, node),
        // Annotations
        "annotations" => lower_annotations(cx, node),
        "annotation" => lower_annotation(cx, node),
        "doc" => lower_doc_block(cx, node),
        "doc_comment" => Doc::text(cx.text(node)),
        "line_comment" => Doc::text(cx.text(node)),
        // Stmts
        "var_decl" => lower_var_decl(cx, node),
        "return_stmt" => lower_return_stmt(cx, node),
        "throw_stmt" => lower_keyword_expr_stmt(cx, node, "throw"),
        "break_stmt" => Doc::text("break;"),
        "continue_stmt" => Doc::text("continue;"),
        "expr_stmt" => lower_expr_stmt(cx, node),
        "if_stmt" => lower_if_stmt(cx, node),
        "while_stmt" => lower_while_stmt(cx, node),
        "do_while_stmt" => lower_do_while_stmt(cx, node),
        "for_stmt" => lower_for_stmt(cx, node),
        "for_in_stmt" => lower_for_in_stmt(cx, node),
        "try_stmt" => lower_try_stmt(cx, node),
        "at_stmt" => lower_at_stmt(cx, node),
        // Expressions
        "binary_expr" => lower_binary_expr(cx, node),
        "unary_expr" => lower_unary_expr(cx, node),
        "paren_expr" => lower_paren_expr(cx, node),
        "tuple_expr" => lower_tuple_expr(cx, node),
        "object_expr" => lower_object_expr(cx, node),
        "object_initializers" => lower_object_initializers(cx, node),
        "object_fields" => lower_object_fields(cx, node),
        "object_field" => lower_object_field(cx, node),
        "array_expr" => lower_array_expr(cx, node),
        "call_expr" => lower_call_expr(cx, node),
        "args" => lower_args_call(cx, node),
        "member_expr" => lower_member_expr(cx, node),
        "arrow_expr" => lower_arrow_expr(cx, node),
        "static_expr" => lower_static_expr(cx, node),
        "offset_expr" => lower_offset_expr(cx, node),
        "lambda_expr" => lower_lambda_expr(cx, node),
        "range_expr" => lower_range_expr(cx, node),
        "interval_expr" => lower_interval_expr(cx, node),
        "type_ident" => lower_type_ident(cx, node),
        "string" => lower_string(cx, node),
        "ident" | "number" | "time" | "char" | "true" | "false" | "null" | "this" | "iso8601" => {
            Doc::text(cx.text(node))
        }
        // Default: walk named children with no inter-child spacing.
        _ => verbatim(cx, node),
    }
}

// -------- Decls --------

fn lower_doc_block<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `doc` is a sequence of `doc_comment` leaves separated by source
    // newlines. Output them one-per-line.
    let mut parts = Vec::new();
    let mut walker = node.walk();
    let mut first = true;
    for c in node.named_children(&mut walker) {
        if !first {
            parts.push(Doc::hardline());
        }
        parts.push(Doc::text(cx.text(c)));
        first = false;
    }
    Doc::concat(parts)
}

fn lower_modifiers<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `private static native` etc. — space-separated, trailing space so
    // the next token (kw) is glued correctly.
    let mut parts = Vec::new();
    let mut walker = node.walk();
    let mut first = true;
    for c in node.children(&mut walker) {
        if !c.is_named() && c.byte_range().is_empty() {
            continue;
        }
        if !first {
            parts.push(Doc::space());
        }
        parts.push(Doc::text(cx.text(c)));
        first = false;
    }
    Doc::concat(parts)
}

fn lower_annotations<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Annotations stack one per line.
    let mut parts = Vec::new();
    let mut walker = node.walk();
    let mut first = true;
    for c in node.named_children(&mut walker) {
        if !first {
            parts.push(Doc::hardline());
        }
        parts.push(lower_node(cx, c));
        first = false;
    }
    Doc::concat(parts)
}

fn lower_annotation<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // @<ident>(args)?
    let mut parts = vec![Doc::text("@")];
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "ident" => parts.push(Doc::text(cx.text(c))),
            "args" => parts.push(lower_args_call(cx, c)),
            _ => parts.push(verbatim(cx, c)),
        }
    }
    Doc::concat(parts)
}

fn lower_mod_pragma<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // <doc?> <annotation>;
    let mut parts = Vec::new();
    if let Some(d) = node.child_by_field_name("doc") {
        parts.push(lower_node(cx, d));
        parts.push(Doc::hardline());
    }
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "doc" => {} // already handled
            "annotation" => parts.push(lower_node(cx, c)),
            _ => parts.push(lower_node(cx, c)),
        }
    }
    parts.push(Doc::text(";"));
    Doc::concat(parts)
}

fn lower_fn_decl<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    parts.push(Doc::text("fn"));
    parts.push(Doc::space());
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    if let Some(g) = node.child_by_field_name("generics") {
        parts.push(lower_node(cx, g));
    }
    if let Some(p) = node.child_by_field_name("params") {
        parts.push(lower_node(cx, p));
    }
    if let Some(r) = node.child_by_field_name("return_type") {
        parts.push(Doc::text(": "));
        parts.push(lower_node(cx, r));
    }
    if let Some(body) = node.child_by_field_name("body") {
        parts.push(Doc::space());
        parts.push(lower_node(cx, body));
    } else {
        parts.push(Doc::text(";"));
    }
    Doc::concat(parts)
}

fn lower_type_decl<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    parts.push(Doc::text("type"));
    parts.push(Doc::space());
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    if let Some(p) = node.child_by_field_name("params") {
        parts.push(lower_node(cx, p));
    }
    if let Some(s) = node.child_by_field_name("supertype") {
        parts.push(Doc::text(" extends "));
        parts.push(lower_node(cx, s));
    }
    if let Some(body) = node.child_by_field_name("body") {
        parts.push(Doc::space());
        parts.push(lower_node(cx, body));
    }
    Doc::concat(parts)
}

fn lower_enum_decl<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    parts.push(Doc::text("enum"));
    parts.push(Doc::space());
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    if let Some(body) = node.child_by_field_name("body") {
        parts.push(Doc::space());
        parts.push(lower_node(cx, body));
    }
    Doc::concat(parts)
}

fn lower_modvar<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    parts.push(Doc::text("var "));
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    parts.push(Doc::text(": "));
    if let Some(ty) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, ty));
    }
    parts.push(Doc::text(";"));
    Doc::concat(parts)
}

/// Common decl-header preamble: walk every leading named child in
/// source order and emit doc / line_comment extras / annotations /
/// modifiers, each terminated with a hardline (or trailing space, in
/// the case of modifiers) so the decl keyword starts cleanly.
///
/// "Leading" stops at the decl keyword (`fn`, `type`, `enum`, `var`)
/// or at the first field-named child (`name`, `params`, etc.). Decls
/// like `mod_pragma` use their own visitor.
fn push_decl_header<'a>(cx: &Cx<'a>, node: Node<'a>, parts: &mut Vec<Doc>) {
    let mut walker = node.walk();
    let mut needs_hardline_before_next = false;
    for c in node.children(&mut walker) {
        if c.is_named() {
            match c.kind() {
                "doc" => {
                    if needs_hardline_before_next {
                        parts.push(Doc::hardline());
                    }
                    parts.push(lower_node(cx, c));
                    needs_hardline_before_next = true;
                    continue;
                }
                "line_comment" => {
                    if needs_hardline_before_next {
                        parts.push(Doc::hardline());
                    }
                    parts.push(Doc::text(cx.text(c)));
                    needs_hardline_before_next = true;
                    continue;
                }
                "annotations" => {
                    if needs_hardline_before_next {
                        parts.push(Doc::hardline());
                    }
                    parts.push(lower_node(cx, c));
                    needs_hardline_before_next = true;
                    continue;
                }
                "modifiers" => {
                    if needs_hardline_before_next {
                        parts.push(Doc::hardline());
                    }
                    parts.push(lower_node(cx, c));
                    parts.push(Doc::space());
                    return;
                }
                _ => break, // hit a structural child → header done
            }
        } else {
            // Anonymous child — typically the decl keyword. Stop here
            // and let the keyword-emit happen in the per-decl visitor.
            // First, terminate the header preamble with a hardline so
            // the keyword starts on its own line.
            break;
        }
    }
    if needs_hardline_before_next {
        parts.push(Doc::hardline());
    }
}

// -------- Bodies --------

fn lower_type_body<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let members: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if members.is_empty() {
        return Doc::text("{}");
    }
    let mut inner = Vec::new();
    let mut prev: Option<Node<'_>> = None;
    for m in &members {
        if let Some(p) = prev {
            // Preserve user blank lines between members.
            let nls = cx.newlines_between(p, *m);
            inner.push(Doc::hardline());
            if nls >= 2 {
                inner.push(Doc::hardline());
            }
        } else {
            inner.push(Doc::hardline());
        }
        inner.push(lower_node(cx, *m));
        prev = Some(*m);
    }
    Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::hardline(),
        Doc::text("}"),
    ])
}

fn lower_enum_body<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let fields: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if fields.is_empty() {
        return Doc::text("{}");
    }
    // Decision: enum bodies render compact-spaced when they fit on one
    // line, multiline otherwise. Emit a Group with `Line` separators
    // and the renderer picks layout. Trailing comma is omitted (the TS
    // reference doesn't add one).
    let mut inner = Vec::new();
    inner.push(Doc::line());
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *f));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ]))
}

fn lower_block<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let stmts: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if stmts.is_empty() {
        // Edge: an empty `{}` may carry an EOL line_comment between
        // braces. Detect it from the source for fixture parity.
        return lower_block_empty(cx, node);
    }
    let lbrace_end = node
        .child(0)
        .map(|c| c.end_byte())
        .unwrap_or(node.start_byte() + 1);
    let mut inner = Vec::new();
    let mut prev_end: usize = lbrace_end;
    for s in &stmts {
        // Recover block comments + extra blank lines from the source
        // gap. The grammar drops `_block_comment` (hidden), so the
        // tree alone doesn't show them.
        let gap_items = scan_gap(cx.source, prev_end..s.start_byte());
        let mut nl_total: u32 = 0;
        for it in gap_items {
            match it {
                GapItem::Newlines(n) => {
                    nl_total += n;
                }
                GapItem::BlockComment(text) => {
                    inner.push(Doc::hardline());
                    if nl_total >= 2 {
                        inner.push(Doc::hardline());
                    }
                    inner.push(Doc::text(text.to_string()));
                    nl_total = 0;
                }
            }
        }
        inner.push(Doc::hardline());
        if nl_total >= 2 {
            inner.push(Doc::hardline());
        }
        inner.push(lower_node(cx, *s));
        prev_end = s.end_byte();
    }
    // Detect EOL comment after `{` for the `eol_comment_spaced` shape:
    // first named child is a `line_comment` whose source position is
    // on the same line as the `{`.
    let first = stmts.first().copied();
    let inline_eol = first.and_then(|c| {
        if c.kind() != "line_comment" {
            return None;
        }
        // Find the `{` token's position.
        let lbrace = node.child(0)?;
        if cx.newlines_between(lbrace, c) == 0 {
            Some(c)
        } else {
            None
        }
    });
    if let Some(eol) = inline_eol {
        // Re-emit: `{` + ` ` + eol_text + Hardline + remaining stmts indented + Hardline + `}`.
        let mut new_inner = Vec::new();
        let mut iter_first = true;
        let mut prev: Option<Node<'_>> = None;
        for s in &stmts {
            if std::ptr::eq(s, &eol) || s.id() == eol.id() {
                continue;
            }
            if iter_first {
                new_inner.push(Doc::hardline());
                iter_first = false;
            } else if let Some(p) = prev {
                let nls = cx.newlines_between(p, *s);
                new_inner.push(Doc::hardline());
                if nls >= 2 {
                    new_inner.push(Doc::hardline());
                }
            }
            new_inner.push(lower_node(cx, *s));
            prev = Some(*s);
        }
        return Doc::concat(vec![
            Doc::text("{ "),
            Doc::text(cx.text(eol).to_string()),
            Doc::indent(Doc::concat(new_inner)),
            Doc::hardline(),
            Doc::text("}"),
        ]);
    }
    Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::hardline(),
        Doc::text("}"),
    ])
}

fn lower_block_empty<'a>(_cx: &Cx<'a>, _node: Node<'a>) -> Doc {
    Doc::text("{}")
}

// -------- Type-body members --------

fn lower_type_attr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    if let Some(t) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, t));
    }
    if let Some(i) = node.child_by_field_name("init") {
        parts.push(Doc::space());
        parts.push(lower_node(cx, i));
    }
    parts.push(Doc::text(";"));
    Doc::concat(parts)
}

fn lower_type_method<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Same skeleton as fn_decl but appears inside a type body.
    let mut parts = Vec::new();
    push_decl_header(cx, node, &mut parts);
    parts.push(Doc::text("fn"));
    parts.push(Doc::space());
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    if let Some(g) = node.child_by_field_name("generics") {
        parts.push(lower_node(cx, g));
    }
    if let Some(p) = node.child_by_field_name("params") {
        parts.push(lower_node(cx, p));
    }
    if let Some(r) = node.child_by_field_name("return_type") {
        parts.push(Doc::text(": "));
        parts.push(lower_node(cx, r));
    }
    if let Some(body) = node.child_by_field_name("body") {
        parts.push(Doc::space());
        parts.push(lower_node(cx, body));
    } else {
        parts.push(Doc::text(";"));
    }
    Doc::concat(parts)
}

fn lower_enum_field<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    Doc::text(cx.text(node))
}

// -------- Params / generics --------

fn lower_fn_params<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let params: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if params.is_empty() {
        return Doc::text("()");
    }
    let mut inner = Vec::new();
    inner.push(Doc::softline());
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *p));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("("),
        Doc::indent(Doc::concat(inner)),
        Doc::softline(),
        Doc::text(")"),
    ]))
}

fn lower_fn_param<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    parts.push(Doc::text(": "));
    // The grammar allows an optional `typeof` keyword between `:` and
    // the type — surface it verbatim.
    let mut walker = node.walk();
    for c in node.children(&mut walker) {
        if !c.is_named() && c.kind() == "typeof" {
            parts.push(Doc::text("typeof "));
        }
    }
    if let Some(t) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, t));
    }
    Doc::concat(parts)
}

fn lower_type_params<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let idents: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if idents.is_empty() {
        return Doc::text("<>");
    }
    let mut inner = Vec::new();
    inner.push(Doc::softline());
    for (i, p) in idents.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *p));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("<"),
        Doc::indent(Doc::concat(inner)),
        Doc::softline(),
        Doc::text(">"),
    ]))
}

fn lower_type_decorator<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `: <type_ident>` for both `attr_type` and `type_decorator`.
    let mut parts = vec![Doc::text(": ")];
    if let Some(t) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, t));
    } else {
        // attr_type's grammar doesn't carry the field name on its
        // type_ident — it's the only named child.
        let mut walker = node.walk();
        for c in node.named_children(&mut walker) {
            parts.push(lower_node(cx, c));
        }
    }
    Doc::concat(parts)
}

fn lower_initializer<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `= <expr>` for both `initializer` and `attr_init`.
    let expr = node
        .child_by_field_name("expr")
        .or_else(|| {
            let mut walker = node.walk();
            node.named_children(&mut walker).next()
        })
        .map(|n| lower_node(cx, n))
        .unwrap_or(Doc::nil());
    Doc::concat(vec![Doc::text("= "), expr])
}

// -------- Args (call-site) --------

fn lower_args_call<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let args: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if args.is_empty() {
        return Doc::text("()");
    }
    let mut inner = Vec::new();
    inner.push(Doc::softline());
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *a));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("("),
        Doc::indent(Doc::concat(inner)),
        Doc::softline(),
        Doc::text(")"),
    ]))
}

// -------- Statements --------

fn lower_var_decl<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("var ")];
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(Doc::text(cx.text(name)));
    }
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "type_decorator" => parts.push(lower_node(cx, c)),
            "initializer" => {
                parts.push(Doc::space());
                parts.push(lower_node(cx, c));
            }
            _ => {}
        }
    }
    parts.push(Doc::text(";"));
    Doc::concat(parts)
}

fn lower_return_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let expr = node.named_children(&mut walker).next();
    if let Some(e) = expr {
        Doc::concat(vec![
            Doc::text("return "),
            lower_node(cx, e),
            Doc::text(";"),
        ])
    } else {
        Doc::text("return;")
    }
}

fn lower_keyword_expr_stmt<'a>(cx: &Cx<'a>, node: Node<'a>, kw: &'static str) -> Doc {
    let mut walker = node.walk();
    let expr = node.named_children(&mut walker).next();
    if let Some(e) = expr {
        Doc::concat(vec![
            Doc::text(format!("{kw} ")),
            lower_node(cx, e),
            Doc::text(";"),
        ])
    } else {
        Doc::text(format!("{kw};"))
    }
}

fn lower_expr_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let expr = node.named_children(&mut walker).next();
    if let Some(e) = expr {
        Doc::concat(vec![lower_node(cx, e), Doc::text(";")])
    } else {
        Doc::text(";")
    }
}

fn lower_if_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("if (")];
    if let Some(cond) = node.child_by_field_name("condition") {
        parts.push(lower_node(cx, cond));
    }
    parts.push(Doc::text(") "));
    if let Some(t) = node.child_by_field_name("then_branch") {
        parts.push(lower_node(cx, t));
    }
    // else_branch is optional, lives via _else_branch hidden rule
    let mut walker = node.walk();
    let mut saw_then = false;
    for c in node.named_children(&mut walker) {
        if !saw_then {
            if Some(c) == node.child_by_field_name("then_branch") {
                saw_then = true;
            }
            continue;
        }
        match c.kind() {
            "if_stmt" => {
                parts.push(Doc::text(" else "));
                parts.push(lower_node(cx, c));
            }
            "block" => {
                parts.push(Doc::text(" else "));
                parts.push(lower_node(cx, c));
            }
            _ => {}
        }
    }
    Doc::concat(parts)
}

fn lower_while_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("while (")];
    if let Some(cond) = node.child_by_field_name("condition") {
        parts.push(lower_node(cx, cond));
    }
    parts.push(Doc::text(") "));
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

fn lower_do_while_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("do ")];
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    parts.push(Doc::text(" while ("));
    if let Some(cond) = node.child_by_field_name("condition") {
        parts.push(lower_node(cx, cond));
    }
    parts.push(Doc::text(");"));
    Doc::concat(parts)
}

fn lower_for_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `for (var i: T = init; cond; incr) <block>`
    // Render verbatim header text — too many small fields and
    // grammar-internal hidden rules to be worth a structured emit at
    // P21.3 maturity. Body uses the structural lowerer.
    let header_end = node.child_by_field_name("block").map(|b| b.start_byte());
    let header = if let Some(end) = header_end {
        // Take everything from start of for_stmt to start of block,
        // then trim trailing whitespace.
        let raw = &cx.source[node.start_byte()..end];
        let trimmed = raw.trim_end();
        trimmed.to_string()
    } else {
        cx.text(node).to_string()
    };
    let mut parts = vec![Doc::text(header), Doc::space()];
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

fn lower_for_in_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Same approach as for_stmt: header is verbatim, body is structural.
    let header_end = node.child_by_field_name("block").map(|b| b.start_byte());
    let header = if let Some(end) = header_end {
        cx.source[node.start_byte()..end].trim_end().to_string()
    } else {
        cx.text(node).to_string()
    };
    let mut parts = vec![Doc::text(header), Doc::space()];
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

fn lower_try_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("try ")];
    if let Some(t) = node.child_by_field_name("try_block") {
        parts.push(lower_node(cx, t));
    }
    parts.push(Doc::text(" catch"));
    if let Some(p) = node.child_by_field_name("error_param") {
        parts.push(Doc::text(" ("));
        parts.push(Doc::text(
            cx.text(p)
                .trim_matches(|c| c == '(' || c == ')')
                .to_string(),
        ));
        parts.push(Doc::text(")"));
    }
    parts.push(Doc::space());
    if let Some(c) = node.child_by_field_name("catch_block") {
        parts.push(lower_node(cx, c));
    }
    Doc::concat(parts)
}

fn lower_at_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("at (")];
    if let Some(e) = node.child_by_field_name("expr") {
        parts.push(lower_node(cx, e));
    }
    parts.push(Doc::text(") "));
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

// -------- Expressions --------

fn lower_binary_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // left <op> right. Op is one of: ?? ^ / * % + - > >= < <= == != as is && || = ?=
    // All take spaces around. `as`/`is` take a `type_ident` on the right.
    let left = node.child_by_field_name("left").map(|n| lower_node(cx, n));
    let right = node.child_by_field_name("right").map(|n| lower_node(cx, n));
    // Operator: scan children for the first anonymous one between left and right.
    let mut op_text: Option<String> = None;
    let mut walker = node.walk();
    for c in node.children(&mut walker) {
        if !c.is_named() && !c.byte_range().is_empty() {
            op_text = Some(cx.text(c).to_string());
            break;
        }
    }
    let op = op_text.unwrap_or_else(|| String::from("?"));
    let mut parts = Vec::new();
    if let Some(l) = left {
        parts.push(l);
    }
    parts.push(Doc::space());
    parts.push(Doc::text(op));
    parts.push(Doc::space());
    if let Some(r) = right {
        parts.push(r);
    }
    Doc::concat(parts)
}

fn lower_unary_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Two shapes: prefix (`-x` / `!x` / `+x` / `*x` / `--x` / `++x`)
    // and postfix (`x--` / `x++` / `x!!`). Distinguish by source order
    // — the operator is anonymous; the operand is the named `_expr`
    // sub-node.
    let mut walker = node.walk();
    let mut leading: Vec<Doc> = Vec::new();
    let mut trailing: Vec<Doc> = Vec::new();
    let mut saw_named = false;
    for c in node.children(&mut walker) {
        if c.is_named() {
            leading.push(lower_node(cx, c));
            saw_named = true;
        } else if !c.byte_range().is_empty() {
            let text = cx.text(c).to_string();
            if saw_named {
                trailing.push(Doc::text(text));
            } else {
                leading.insert(leading.len().saturating_sub(0), Doc::text(text));
            }
        }
    }
    // Reorder: `leading` already has prefix-op then operand in source
    // order if the prefix came first; the `insert` above is a no-op
    // because we only encountered ops before any named child.
    let mut parts = Vec::new();
    parts.extend(leading);
    parts.extend(trailing);
    Doc::concat(parts)
}

fn lower_paren_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let inner = node
        .child_by_field_name("expr")
        .map(|n| lower_node(cx, n))
        .unwrap_or(Doc::nil());
    Doc::concat(vec![Doc::text("("), inner, Doc::text(")")])
}

fn lower_tuple_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let l = node.child_by_field_name("left").map(|n| lower_node(cx, n));
    let r = node.child_by_field_name("right").map(|n| lower_node(cx, n));
    let mut parts = vec![Doc::text("(")];
    if let Some(l) = l {
        parts.push(l);
    }
    parts.push(Doc::text(", "));
    if let Some(r) = r {
        parts.push(r);
    }
    parts.push(Doc::text(")"));
    Doc::concat(parts)
}

fn lower_object_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `<type> { ... }` — type ident followed by either object_initializers or object_fields.
    let mut parts = Vec::new();
    if let Some(t) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, t));
        parts.push(Doc::space());
    }
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "object_initializers" | "object_fields" => parts.push(lower_node(cx, c)),
            _ => {}
        }
    }
    Doc::concat(parts)
}

fn lower_object_initializers<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let inits: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if inits.is_empty() {
        return Doc::text("{}");
    }
    // `{ a, b, c }` — fields_spaced when fits, multiline otherwise.
    let mut inner = Vec::new();
    inner.push(Doc::line());
    for (i, e) in inits.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *e));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ]))
}

fn lower_object_fields<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let fields: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if fields.is_empty() {
        return Doc::text("{}");
    }
    let mut inner = Vec::new();
    inner.push(Doc::line());
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *f));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ]))
}

fn lower_object_field<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    if let Some(name) = node.child_by_field_name("name") {
        parts.push(lower_node(cx, name));
    }
    parts.push(Doc::text(": "));
    if let Some(value) = node.child_by_field_name("value") {
        parts.push(lower_node(cx, value));
    }
    Doc::concat(parts)
}

fn lower_array_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let elems: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if elems.is_empty() {
        return Doc::text("[]");
    }
    let mut inner = Vec::new();
    inner.push(Doc::softline());
    for (i, e) in elems.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *e));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("["),
        Doc::indent(Doc::concat(inner)),
        Doc::softline(),
        Doc::text("]"),
    ]))
}

fn lower_call_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // <fn> <args>
    let mut parts = Vec::new();
    if let Some(f) = node.child_by_field_name("fn") {
        parts.push(lower_node(cx, f));
    }
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        if c.kind() == "args" {
            parts.push(lower_args_call(cx, c));
            break;
        }
    }
    Doc::concat(parts)
}

fn lower_member_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_dot_arrow_expr(cx, node, ".")
}

fn lower_arrow_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_dot_arrow_expr(cx, node, "->")
}

fn lower_dot_arrow_expr<'a>(cx: &Cx<'a>, node: Node<'a>, sep: &'static str) -> Doc {
    // `<recv> <pre_optional?> . <property> <post_optional?>`
    // Children in source order: receiver (named), optional `?` (named),
    // `.` (anon), property (named ident), optional `?` (named).
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    let mut emitted_sep = false;
    for c in node.children(&mut walker) {
        if !c.is_named() && !c.byte_range().is_empty() {
            // The `.` or `->` operator.
            parts.push(Doc::text(sep.to_string()));
            emitted_sep = true;
            continue;
        }
        if !c.is_named() {
            continue;
        }
        // Named children: receiver, optional `?`, property ident.
        if c.kind() == "optional" {
            // Render the literal `?` token as-is.
            parts.push(Doc::text("?"));
        } else if !emitted_sep {
            parts.push(lower_node(cx, c));
        } else {
            // Property ident.
            parts.push(Doc::text(cx.text(c)));
        }
    }
    Doc::concat(parts)
}

fn lower_static_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `<recv>::<property>` — tight, no spaces.
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    let mut saw_recv = false;
    for c in node.children(&mut walker) {
        if c.is_named() {
            if !saw_recv {
                parts.push(lower_node(cx, c));
                saw_recv = true;
            } else {
                parts.push(Doc::text(cx.text(c)));
            }
        } else if !c.byte_range().is_empty() {
            // The `::` operator.
            parts.push(Doc::text("::"));
        }
    }
    Doc::concat(parts)
}

fn lower_offset_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `<recv>?[<index>]?`
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    let mut state = 0u8; // 0=before [, 1=after [, 2=after ]
    for c in node.children(&mut walker) {
        if c.is_named() {
            if c.kind() == "optional" {
                parts.push(Doc::text("?"));
            } else {
                parts.push(lower_node(cx, c));
            }
        } else if !c.byte_range().is_empty() {
            let t = cx.text(c);
            match t {
                "[" => {
                    parts.push(Doc::text("["));
                    state = 1;
                }
                "]" => {
                    parts.push(Doc::text("]"));
                    state = 2;
                }
                _ => parts.push(Doc::text(t.to_string())),
            }
        }
    }
    let _ = state;
    Doc::concat(parts)
}

fn lower_lambda_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("fn")];
    if let Some(p) = node.child_by_field_name("params") {
        parts.push(lower_node(cx, p));
    }
    parts.push(Doc::space());
    if let Some(b) = node.child_by_field_name("body") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

fn lower_range_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // <from?>..<to?> — tight `..`.
    let from = node.child_by_field_name("from").map(|n| lower_node(cx, n));
    let to = node.child_by_field_name("to").map(|n| lower_node(cx, n));
    let mut parts = Vec::new();
    if let Some(f) = from {
        parts.push(f);
    }
    parts.push(Doc::text(".."));
    if let Some(t) = to {
        parts.push(t);
    }
    Doc::concat(parts)
}

fn lower_interval_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // <[|]><from?>..<to?><[|]>
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    for c in node.children(&mut walker) {
        if c.is_named() {
            parts.push(lower_node(cx, c));
        } else if !c.byte_range().is_empty() {
            parts.push(Doc::text(cx.text(c).to_string()));
        }
    }
    Doc::concat(parts)
}

fn lower_type_ident<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `<typeof?> <ns0::ns1::..::>name<<args>>?<?>`
    // Children in source order: optional "typeof", repeating ident+"::"
    // segments, name ident, optional `<` ... `>` with type_ident
    // params, optional `?`.
    //
    // We walk `node.children` to preserve order and emit each piece
    // with appropriate spacing (tight everywhere except `typeof`).
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    // First emit any `typeof` keyword (anonymous), then idents/`::`,
    // then `<...>` if present, then optional `?`.
    let children: Vec<Node<'_>> = node.children(&mut walker).collect();
    let mut i = 0;
    while i < children.len() {
        let c = children[i];
        if c.is_named() {
            match c.kind() {
                "ident" => parts.push(Doc::text(cx.text(c))),
                "type_ident" => parts.push(lower_node(cx, c)),
                "optional" => parts.push(Doc::text("?")),
                _ => parts.push(verbatim(cx, c)),
            }
        } else if !c.byte_range().is_empty() {
            let t = cx.text(c);
            match t {
                "typeof" => {
                    parts.push(Doc::text("typeof "));
                }
                "::" => parts.push(Doc::text("::")),
                "," => parts.push(Doc::text(", ")),
                "<" => parts.push(Doc::text("<")),
                ">" => parts.push(Doc::text(">")),
                _ => parts.push(Doc::text(t.to_string())),
            }
        }
        i += 1;
    }
    Doc::concat(parts)
}

fn lower_string<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Strings are emitted verbatim — preserving every byte, including
    // multi-line content and any embedded `${...}` interpolations.
    // The grammar's string nodes wrap their bytes faithfully.
    Doc::text(cx.text(node).to_string())
}

// -------- Verbatim fallback --------

/// Emit the node's source text verbatim. Used for kinds the lowering
/// hasn't been taught yet — keeps output legal and idempotent on
/// re-format, at the cost of in-construct normalization.
fn verbatim<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    Doc::text(cx.text(node).to_string())
}
