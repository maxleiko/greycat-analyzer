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

use crate::directives::FmtDirectives;
use crate::doc::Doc;
use greycat_analyzer_syntax::tree_sitter::Node;

/// Lowering context — owns the source, threads through every visitor.
pub struct Cx<'a> {
    source: &'a str,
    directives: FmtDirectives,
}

impl<'a> Cx<'a> {
    pub fn new(source: &'a str) -> Self {
        Cx {
            source,
            directives: FmtDirectives::empty(),
        }
    }

    // P23.4
    /// Build a context whose lowerer honors `// gcl-fmt-…`
    /// directives parsed from `source`. Used by [`crate::format`] /
    /// [`crate::format_tree`] / [`crate::format_with`] /
    /// [`crate::format_tree_with`] so the verbatim regions land in the
    /// output. External callers driving [`Cx`] directly can pass their
    /// own pre-parsed directive set.
    pub fn with_directives(source: &'a str, directives: FmtDirectives) -> Self {
        Cx { source, directives }
    }

    pub fn text(&self, node: Node<'_>) -> &'a str {
        &self.source[node.byte_range()]
    }

    // P23.4
    /// `true` when the lowerer should emit `node`'s source
    /// bytes verbatim instead of recursing.
    pub fn skip_for_fmt(&self, node: Node<'_>) -> bool {
        self.directives.is_skipped(&node.byte_range())
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
///
/// `block_comment` is a named extra in the grammar (alongside
/// `line_comment`), so it appears in `named_children()` between decls
/// and is emitted naturally — no source-byte gap-scanning needed.
pub fn lower_module<'a>(cx: &Cx<'a>, root: Node<'a>) -> Doc {
    let mut parts: Vec<Doc> = Vec::new();
    let mut prev: Option<Node<'a>> = None;
    let mut walker = root.walk();
    for child in root.named_children(&mut walker) {
        if let Some(p) = prev {
            // Source-driven blank-line preservation: count newlines in
            // the gap and emit (count) hardlines so the user's vertical
            // spacing survives, capped at 4 (one terminator + 3 blanks).
            let nls = cx.newlines_between(p, child).min(4);
            if nls == 0 {
                // Zero newlines — the child shares a source line with
                // `p`. The common case is an EOL trailing comment after
                // a top-level decl (`fn foo() {} // note` or
                // `fn foo() {} /* note */`); emit a single space so the
                // comment stays on the same line. Other on-same-line
                // top-level constructs (two decls on one line) get
                // forced onto separate lines for legibility.
                if matches!(child.kind(), "line_comment" | "block_comment") {
                    parts.push(Doc::text(" "));
                } else {
                    parts.push(Doc::hardline());
                }
            } else {
                for _ in 0..nls {
                    parts.push(Doc::hardline());
                }
            }
        }
        parts.push(lower_node(cx, child));
        prev = Some(child);
    }
    Doc::concat(parts)
}

/// Dispatch on `node.kind()`. Unknown kinds fall through to `verbatim`.
fn lower_node<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // **P23.4** — `// gcl-fmt-off` / `// gcl-fmt-skip` regions: emit
    // the node's source bytes unchanged instead of recursing into the
    // CST. Checked at the dispatcher entry so every node-kind honors
    // it without per-arm plumbing.
    if cx.skip_for_fmt(node) {
        return Doc::text(cx.text(node));
    }
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
        "block_comment" => Doc::text(cx.text(node)),
        // Stmts
        "var_decl" => lower_var_decl(cx, node),
        "return_stmt" => lower_return_stmt(cx, node),
        "throw_stmt" => lower_keyword_expr_stmt(cx, node, "throw"),
        "break_stmt" => Doc::text("break;"),
        "continue_stmt" => Doc::text("continue;"),
        "breakpoint_stmt" => Doc::text("breakpoint;"),
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
        parts.push(Doc::group(Doc::indent(Doc::concat(vec![
            Doc::line(),
            Doc::text("extends "),
            lower_node(cx, s),
        ]))));
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
                "line_comment" | "block_comment" => {
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
    // Seed `prev` with the opening `{` so an EOL `line_comment` /
    // `block_comment` on the same source line as `{` is detected and
    // glued with a single space instead of demoted.
    let mut inner = Vec::new();
    let mut prev: Option<Node<'_>> = node.child(0);
    let mut seen_member = false;
    for m in &members {
        let is_comment = matches!(m.kind(), "line_comment" | "block_comment");
        if is_comment
            && let Some(p) = prev
            && cx.newlines_between(p, *m) == 0
        {
            // EOL trailing comment — glue with one space.
            inner.push(Doc::text(" "));
            inner.push(Doc::text(cx.text(*m).to_string()));
            prev = Some(*m);
            seen_member = true;
            continue;
        }
        if seen_member && let Some(p) = prev {
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
        seen_member = true;
    }
    Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::hardline(),
        Doc::text("}"),
    ])
}

fn lower_enum_body<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Always multi-line, like the TS reference's `rules.stmts` indent.
    // Walk every child in source order so the separator between fields
    // (either `,` or `;`) survives verbatim, and so a leading
    // `line_comment` / `block_comment` / `doc_comment` extra binds to
    // the next field on its own line.
    let mut walker = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut walker).collect();
    let has_field = children.iter().any(|c| c.kind() == "enum_field");
    if !has_field {
        return Doc::text("{}");
    }
    let mut inner: Vec<Doc> = Vec::new();
    let mut pending_sep: Option<&'a str> = None;
    let mut needs_hardline = true; // first item on its own line
    let mut prev_node: Option<Node<'_>> = None;
    for c in &children {
        let kind = c.kind();
        if kind == "{" || kind == "}" {
            prev_node = Some(*c);
            continue;
        }
        match kind {
            "enum_field" => {
                if let Some(sep) = pending_sep.take() {
                    inner.push(Doc::text(sep.to_string()));
                }
                if needs_hardline {
                    inner.push(Doc::hardline());
                }
                inner.push(lower_node(cx, *c));
                needs_hardline = true;
            }
            "line_comment" | "block_comment" => {
                // EOL trailing comment on the same source line as
                // whatever came before (typically the trailing `,` or
                // `;`): glue it with a single space instead of demoting
                // it. The pending separator is emitted first so the
                // comment lands as `Field, // text` or `Field, /* … */`.
                let is_eol = prev_node.is_some_and(|p| cx.newlines_between(p, *c) == 0);
                if let Some(sep) = pending_sep.take() {
                    inner.push(Doc::text(sep.to_string()));
                }
                if is_eol {
                    inner.push(Doc::text(" "));
                    inner.push(Doc::text(cx.text(*c).to_string()));
                } else {
                    if needs_hardline {
                        inner.push(Doc::hardline());
                    }
                    inner.push(Doc::text(cx.text(*c).to_string()));
                }
                needs_hardline = true;
            }
            "doc_comment" => {
                if let Some(sep) = pending_sep.take() {
                    inner.push(Doc::text(sep.to_string()));
                }
                if needs_hardline {
                    inner.push(Doc::hardline());
                }
                inner.push(Doc::text(cx.text(*c).to_string()));
                needs_hardline = true;
            }
            "," => {
                pending_sep = Some(",");
            }
            ";" | "extra_semi" => {
                pending_sep = Some(";");
            }
            "_semi" => {
                pending_sep = Some(";");
            }
            _ => {}
        }
        prev_node = Some(*c);
    }
    // Final separator: TS reference prints whatever was last in source.
    // If the source had no terminator after the last field, we don't
    // add one either — `enum_body`'s grammar lets the trailing
    // separator be optional and the formatter's job is to preserve it
    // verbatim, not to canonicalize.
    if let Some(sep) = pending_sep {
        inner.push(Doc::text(sep.to_string()));
    }
    Doc::concat(vec![
        Doc::text("{"),
        Doc::indent(Doc::concat(inner)),
        Doc::hardline(),
        Doc::text("}"),
    ])
}

fn lower_block<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let stmts: Vec<Node<'_>> = node.named_children(&mut walker).collect();
    if stmts.is_empty() {
        return lower_block_empty(cx, node);
    }
    // Seed `prev` with the opening `{` so an EOL `line_comment` /
    // `block_comment` on the same source line as `{` is glued with one
    // space instead of demoted.
    let mut inner = Vec::new();
    let mut prev: Option<Node<'_>> = node.child(0);
    let mut seen_stmt = false;
    for s in &stmts {
        let is_comment = matches!(s.kind(), "line_comment" | "block_comment");
        if is_comment
            && let Some(p) = prev
            && cx.newlines_between(p, *s) == 0
        {
            // EOL trailing comment — glue with exactly one space.
            inner.push(Doc::text(" "));
            inner.push(Doc::text(cx.text(*s).to_string()));
            prev = Some(*s);
            seen_stmt = true;
            continue;
        }
        if seen_stmt && let Some(p) = prev {
            let nls = cx.newlines_between(p, *s);
            inner.push(Doc::hardline());
            if nls >= 2 {
                inner.push(Doc::hardline());
            }
        } else {
            inner.push(Doc::hardline());
        }
        inner.push(lower_node(cx, *s));
        prev = Some(*s);
        seen_stmt = true;
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
    // `: <type_ident>` for both `attr_type` and `type_decorator`. With
    // `block_comment` as a named extra, the fallback "first named
    // child" lookup must skip comments so it lands on the real type.
    let mut parts = vec![Doc::text(": ")];
    let type_node = node.child_by_field_name("type").or_else(|| {
        let mut walker = node.walk();
        node.named_children(&mut walker)
            .find(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
    });
    if let Some(t) = type_node {
        parts.push(lower_node(cx, t));
    }
    Doc::concat(parts)
}

fn lower_initializer<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `= <expr>` for both `initializer` and `attr_init`. The fallback
    // "first named child" lookup must skip comments so it lands on the
    // real expr.
    let expr = node.child_by_field_name("expr").or_else(|| {
        let mut walker = node.walk();
        node.named_children(&mut walker)
            .find(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
    });
    let expr_doc = expr.map(|n| lower_node(cx, n)).unwrap_or(Doc::nil());
    Doc::concat(vec![Doc::text("= "), expr_doc])
}

// -------- Args (call-site) --------

fn lower_args_call<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    // Filter out comments — they'd otherwise be picked up as "args"
    // and emitted in place of an actual argument. Inline-comment
    // recovery at this site is left as future work; for now any
    // file with `/* */` inside call args falls back to verbatim via
    // the safety net in `format_tree_with`.
    let args: Vec<Node<'_>> = node
        .named_children(&mut walker)
        .filter(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
        .collect();
    if args.is_empty() {
        return Doc::text("()");
    }
    // Trailing-comma opt-in: a source-level `,` after the last arg
    // forces the args group into multi-line mode and preserves the
    // hint on output, mirroring the same idiom on object_fields.
    let trailing = has_trailing_comma(node);
    let mut inner = Vec::new();
    inner.push(if trailing {
        Doc::hardline()
    } else {
        Doc::softline()
    });
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *a));
    }
    if trailing {
        inner.push(Doc::text(","));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("("),
        Doc::indent_if_broken(Doc::concat(inner)),
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
    // Pick the first non-comment named child as the expr.
    let mut walker = node.walk();
    let expr = node
        .named_children(&mut walker)
        .find(|c| !matches!(c.kind(), "block_comment" | "line_comment"));
    if let Some(e) = expr {
        Doc::concat(vec![
            Doc::text("return "),
            wrap_keyword_expr(cx, e),
            Doc::text(";"),
        ])
    } else {
        Doc::text("return;")
    }
}

fn lower_keyword_expr_stmt<'a>(cx: &Cx<'a>, node: Node<'a>, kw: &'static str) -> Doc {
    let mut walker = node.walk();
    let expr = node
        .named_children(&mut walker)
        .find(|c| !matches!(c.kind(), "block_comment" | "line_comment"));
    if let Some(e) = expr {
        Doc::concat(vec![
            Doc::text(format!("{kw} ")),
            wrap_keyword_expr(cx, e),
            Doc::text(";"),
        ])
    } else {
        Doc::text(format!("{kw};"))
    }
}

/// Lower an expression that follows a statement keyword (`return`,
/// `throw`). The expression is emitted verbatim — **never** wrapped in
/// an outer Group with a leading softline, regardless of width.
///
/// **ASI safety:** GreyCat has automatic semicolon insertion
/// (`_automatic_semicolon` is an external token in the grammar's
/// `return_stmt` / `throw_stmt` rules). A newline between `return` and
/// its expression re-parses as `return;` followed by a separate
/// statement — silent semantic corruption, the classic JS
/// "`return\n{ ... }`" bug. Even when the formatter's first pass
/// happens to render `return\n    expr;` correctly (because the user's
/// source typed it on one line), the *second* pass would re-tokenize
/// from the formatted output and produce different code.
///
/// Expressions whose own lowering can break (chains, brace-led
/// constructors, postfix chains, …) handle overflow internally —
/// `return foo(\n    arg1,\n    arg2\n);` keeps `return` glued to
/// `foo(` and only the args break. Atomic expressions that can't break
/// internally (string literals, plain idents, …) stay on one long line
/// when they overflow; the user can break them manually via string
/// concat or similar, but the formatter never inserts a break that
/// could be re-tokenized away.
fn wrap_keyword_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_node(cx, node)
}

/// True when the expression's lowering already produces a `Doc::Group`
/// that can break across lines on its own. Used by paren-wrapping
/// callers (if/while/return/…) to avoid stacking a redundant outer
/// Group on top of an inner break.
fn is_self_wrapping_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> bool {
    match node.kind() {
        "binary_expr" => binary_op_text(cx, node)
            .and_then(|op| chain_group(&op))
            .is_some(),
        // Brace-led constructs already wrap themselves in a Group at the
        // `{` / `[` so they can break internally. Wrapping again under
        // `return` / `throw` would demote the leading brace onto its own
        // line ("return\n    Obj {…}"); pass them through instead so the
        // open brace stays glued to the keyword.
        "object_expr" => true,
        _ => false,
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
        parts.push(wrap_paren_condition(cx, cond));
    }
    parts.push(Doc::text(") "));
    if let Some(t) = node.child_by_field_name("then_branch") {
        parts.push(lower_node(cx, t));
    }
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
        parts.push(wrap_paren_condition(cx, cond));
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
        parts.push(wrap_paren_condition(cx, cond));
    }
    parts.push(Doc::text(");"));
    Doc::concat(parts)
}

/// Wrap a condition expression (the body of `if (…)`, `while (…)`,
/// `at (…)`, `do { } while (…)`) so the closing `)` lands on its own
/// line at the statement's indent when the condition would overflow.
///
/// Two shapes:
/// - Chain-style conditions (`a && b && c`) glue the first operand to
///   the opening `(`, indent the continuation through the chain's own
///   `Doc::indent`, and emit `Doc::if_broken(Doc::line())` to push `)`
///   onto a fresh line at the outer indent only when the chain breaks.
///   Without this trailing break, `) {` would glue to the last operand
///   — and hitting Enter inside the if's `{` body in an editor would
///   add one indent too many.
/// - Non-chain conditions follow the symmetric "softline before and
///   after" shape so the whole condition floats onto its own indented
///   line when wrapped.
fn wrap_paren_condition<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    if is_self_wrapping_expr(cx, node) {
        Doc::group(Doc::concat(vec![
            lower_node(cx, node),
            Doc::if_broken(Doc::line()),
        ]))
    } else {
        Doc::group(Doc::concat(vec![
            Doc::indent(Doc::concat(vec![Doc::softline(), lower_node(cx, node)])),
            Doc::softline(),
        ]))
    }
}

fn lower_for_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `for (var <name>[: <type>] = <init>; <cond>; <incr>) <block>`
    let it_name = node
        .child_by_field_name("it_name")
        .map(|n| cx.text(n).to_string())
        .unwrap_or_default();
    // The `it_type` field grammar is `optional(seq(":", $.type_ident))`,
    // and tree-sitter applies the field name to *every* element of the
    // seq — so `child_by_field_name("it_type")` returns the `:` token,
    // not the type_ident. Pick the named type_ident directly instead.
    let it_type = named_child_by_field(node, "it_type").map(|n| lower_node(cx, n));
    let it_value = node
        .child_by_field_name("it_value")
        .map(|n| lower_node(cx, n));
    let it_condition = node
        .child_by_field_name("it_condition")
        .map(|n| lower_node(cx, n));
    let it_increment = node
        .child_by_field_name("it_increment")
        .map(|n| lower_node(cx, n));

    let mut init = vec![Doc::text("var "), Doc::text(it_name)];
    if let Some(t) = it_type {
        init.push(Doc::text(": "));
        init.push(t);
    }
    init.push(Doc::text(" = "));
    if let Some(v) = it_value {
        init.push(v);
    }

    let header = Doc::concat(vec![
        Doc::concat(init),
        Doc::text(";"),
        Doc::line(),
        it_condition.unwrap_or(Doc::nil()),
        Doc::text(";"),
        Doc::line(),
        it_increment.unwrap_or(Doc::nil()),
    ]);

    let mut parts = vec![
        Doc::text("for ("),
        Doc::group(Doc::concat(vec![
            Doc::indent(Doc::concat(vec![Doc::softline(), header])),
            Doc::softline(),
        ])),
        Doc::text(") "),
    ];
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

fn lower_for_in_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // `for (<param>[, <param>...] in <iter>[?][<range>][sampling X][limit Y][skip Z]) <block>`
    let mut params: Vec<Node<'_>> = Vec::new();
    let mut iter_optional = false;
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "for_in_param" => params.push(c),
            "optional" => iter_optional = true,
            _ => {}
        }
    }

    let iterator = node
        .child_by_field_name("iterator")
        .map(|n| lower_node(cx, n));
    let range = node.child_by_field_name("range").map(|n| lower_node(cx, n));
    // `sampling` / `limit` / `skip` fields wrap a seq with the keyword
    // and the expr — same trap as `it_type` on for_stmt: the field
    // tags both children, and `child_by_field_name` returns the keyword
    // token. Pick the named expr child directly.
    let sampling = named_child_by_field(node, "sampling").map(|n| lower_node(cx, n));
    let limit = named_child_by_field(node, "limit").map(|n| lower_node(cx, n));
    let skip = named_child_by_field(node, "skip").map(|n| lower_node(cx, n));

    let mut header: Vec<Doc> = Vec::new();
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            header.push(Doc::text(","));
            header.push(Doc::line());
        }
        header.push(lower_for_in_param(cx, *p));
    }
    header.push(Doc::text(" in "));
    if let Some(it) = iterator {
        header.push(it);
    }
    if iter_optional {
        header.push(Doc::text("?"));
    }
    // The `range` interval_expr renders with its own `[`/`]` (or `]`/`[`)
    // brackets included — glue it to the iterator with no separating space,
    // mirroring how `arr[from..to]` reads as a subscript form.
    if let Some(r) = range {
        header.push(r);
    }
    if let Some(s) = sampling {
        header.push(Doc::text(" sampling "));
        header.push(s);
    }
    if let Some(l) = limit {
        header.push(Doc::text(" limit "));
        header.push(l);
    }
    if let Some(sk) = skip {
        header.push(Doc::text(" skip "));
        header.push(sk);
    }

    let mut parts = vec![
        Doc::text("for ("),
        Doc::group(Doc::concat(vec![
            Doc::indent(Doc::concat(vec![Doc::softline(), Doc::concat(header)])),
            Doc::softline(),
        ])),
        Doc::text(") "),
    ];
    if let Some(b) = node.child_by_field_name("block") {
        parts.push(lower_node(cx, b));
    }
    Doc::concat(parts)
}

/// `child_by_field_name` matches the *first* child with the given
/// field name, but when the grammar applies a field to a wrapping
/// `seq(token, $.named_node)` (as in for_stmt's `it_type` or for_in_stmt's
/// `sampling` / `limit` / `skip`), tree-sitter tags both children with
/// the field name and returns the keyword/punctuation first. Walking
/// the cursor and filtering to named children with the matching field
/// gives the actual content node.
fn named_child_by_field<'a>(node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        if cursor.node().is_named() && cursor.field_name() == Some(name) {
            return Some(cursor.node());
        }
        if !cursor.goto_next_sibling() {
            return None;
        }
    }
}

fn lower_for_in_param<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let name = node
        .child_by_field_name("name")
        .map(|n| cx.text(n).to_string())
        .unwrap_or_default();
    let ty = node.child_by_field_name("type").map(|n| lower_node(cx, n));
    let mut parts = vec![Doc::text(name)];
    if let Some(t) = ty {
        parts.push(Doc::text(": "));
        parts.push(t);
    }
    Doc::concat(parts)
}

fn lower_try_stmt<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = vec![Doc::text("try ")];
    if let Some(t) = node.child_by_field_name("try_block") {
        parts.push(lower_node(cx, t));
    }
    parts.push(Doc::text(" catch"));
    // Three accepted shapes: `catch`, `catch (e)`, `catch ()`. The third is
    // grammar-permissive so partial edits don't surface as ERROR nodes —
    // the formatter normalizes it to the bare `catch` form since no ident
    // is bound either way.
    if let Some(p) = node.child_by_field_name("error_param") {
        parts.push(Doc::text(" ("));
        parts.push(Doc::text(cx.text(p).to_string()));
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
        parts.push(wrap_paren_condition(cx, e));
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
    let op = binary_op_text(cx, node).unwrap_or_else(|| String::from("?"));

    // Flatten left-associative chains of same-precedence operators into one
    // Group, so the renderer can break before each operator onto its own
    // continuation line (leading-operator style). Mixed precedence stays
    // safe because the walk only descends while the left child's operator
    // is in the *same* group — e.g. `a && b || c` (parsed as `(a && b) || c`)
    // walks the outer `||` chain and treats the inner `&&` subtree as one
    // atomic operand.
    if let Some(group) = chain_group(&op) {
        let mut head: Option<Doc> = None;
        let mut segments: Vec<(String, Doc)> = Vec::new();
        collect_op_chain(cx, node, group, &mut head, &mut segments);
        if let Some(head_doc) = head
            && segments.len() >= 2
        {
            let mut tail: Vec<Doc> = Vec::new();
            for (seg_op, operand) in segments {
                tail.push(Doc::line());
                tail.push(Doc::text(seg_op));
                tail.push(Doc::space());
                tail.push(operand);
            }
            return Doc::group(Doc::concat(vec![head_doc, Doc::indent(Doc::concat(tail))]));
        }
    }

    let left = node
        .child_by_field_name("left")
        .map(|n| lower_node(cx, n))
        .unwrap_or(Doc::nil());
    let right = node
        .child_by_field_name("right")
        .map(|n| lower_node(cx, n))
        .unwrap_or(Doc::nil());
    // Assignment ops (`=`, `?=`) keep the binop Group as a break
    // opportunity (so long chain-only assignments split at `=` rather
    // than fragmenting at chain dots), but use `Doc::indent_if_broken`
    // instead of `Doc::indent` for the continuation. The distinction
    // matters when the right-side expression is self-wrapping via
    // `Doc::expand` (object initializers, etc.): the binop Group's
    // fit-check sees zero width from the expanded child, the Group
    // stays flat, and `indent_if_broken` contributes no indent step —
    // so the right-side's own internal break renders at the surrounding
    // block's indent. The previous `Doc::indent` always added a step,
    // leaving brace-led RHSs over-indented by one level.
    //
    // When the right side is a flat chain or simple expr that doesn't
    // fit, the binop Group breaks, `indent_if_broken` adds the
    // continuation-indent step, and the layout matches the original
    // "break at the operator first" semantics.
    if op == "=" || op == "?=" {
        return Doc::group(Doc::concat(vec![
            left,
            Doc::indent_if_broken(Doc::concat(vec![
                Doc::line(),
                Doc::text(op),
                Doc::space(),
                right,
            ])),
        ]));
    }
    // Wrap remaining non-chain binops (`==`, `!=`, `<`, `>`, `<=`, `>=`,
    // `as`, `is`) in their own Group with a `Line` break-point before
    // the operator. This makes the operator the outer break boundary —
    // when the surrounding expression overflows, the binop breaks at the
    // operator first, and any nested member/arrow chain inside `left` /
    // `right` re-measures with the tighter column and stays flat when
    // it fits. Without this group, the chain group is the only break
    // opportunity and it fragments at every `.` instead.
    Doc::group(Doc::concat(vec![
        left,
        Doc::indent(Doc::concat(vec![
            Doc::line(),
            Doc::text(op),
            Doc::space(),
            right,
        ])),
    ]))
}

/// Extract the operator text from a `binary_expr` — the first non-empty
/// anonymous child between `left` and `right`.
fn binary_op_text<'a>(cx: &Cx<'a>, node: Node<'a>) -> Option<String> {
    let mut walker = node.walk();
    for c in node.children(&mut walker) {
        if !c.is_named() && !c.byte_range().is_empty() {
            return Some(cx.text(c).to_string());
        }
    }
    None
}

/// Precedence group for chain-wrappable binary operators. Same-group
/// operators flatten into one Group; cross-group boundaries become
/// atomic operands. Comparison / `as` / `is` / `=` / `?=` aren't
/// chainable (or have non-expr right operands) and stay out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChainGroup {
    LogicalOr,
    LogicalAnd,
    Nullish,
    Additive,
    Multiplicative,
    Xor,
}

fn chain_group(op: &str) -> Option<ChainGroup> {
    match op {
        "||" => Some(ChainGroup::LogicalOr),
        "&&" => Some(ChainGroup::LogicalAnd),
        "??" => Some(ChainGroup::Nullish),
        "+" | "-" => Some(ChainGroup::Additive),
        "*" | "/" | "%" => Some(ChainGroup::Multiplicative),
        "^" => Some(ChainGroup::Xor),
        _ => None,
    }
}

/// Walk the left spine of a same-group operator chain and push
/// `(op, operand)` pairs in source order into `out`. The chain's head
/// operand (the deepest left leaf that isn't a same-group binary_expr)
/// goes into `out_head`. Stops descending whenever the left child isn't
/// a `binary_expr` with an operator in the same group — that subtree is
/// then treated as one atomic operand.
fn collect_op_chain<'a>(
    cx: &Cx<'a>,
    node: Node<'a>,
    group: ChainGroup,
    out_head: &mut Option<Doc>,
    out_segments: &mut Vec<(String, Doc)>,
) {
    if let Some(left) = node.child_by_field_name("left") {
        if left.kind() == "binary_expr"
            && binary_op_text(cx, left).and_then(|op| chain_group(&op)) == Some(group)
        {
            collect_op_chain(cx, left, group, out_head, out_segments);
        } else {
            *out_head = Some(lower_node(cx, left));
        }
    }
    let op = binary_op_text(cx, node).unwrap_or_else(|| String::from("?"));
    if let Some(right) = node.child_by_field_name("right") {
        out_segments.push((op, lower_node(cx, right)));
    }
}

fn lower_unary_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    // Prefix (`-x`, `!x`, `++x`) or postfix (`x++`, `x!!`). The
    // operator is anonymous; the operand is the only named child
    // (filter out `block_comment` / `line_comment` since they're now
    // named extras).
    let mut walker = node.walk();
    let mut parts: Vec<Doc> = Vec::new();
    for c in node.children(&mut walker) {
        if c.is_named() {
            if !matches!(c.kind(), "block_comment" | "line_comment") {
                parts.push(lower_node(cx, c));
            }
        } else if !c.byte_range().is_empty() {
            parts.push(Doc::text(cx.text(c).to_string()));
        }
    }
    Doc::concat(parts)
}

fn lower_paren_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let inner = node
        .child_by_field_name("expr")
        .map(|n| lower_node(cx, n))
        .unwrap_or(Doc::nil());
    Doc::group(Doc::concat(vec![
        Doc::text("("),
        Doc::indent(Doc::concat(vec![Doc::softline(), inner])),
        Doc::softline(),
        Doc::text(")"),
    ]))
}

fn lower_tuple_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let l = node.child_by_field_name("left").map(|n| lower_node(cx, n));
    let r = node.child_by_field_name("right").map(|n| lower_node(cx, n));
    let mut inner = vec![Doc::softline()];
    if let Some(l) = l {
        inner.push(l);
    }
    inner.push(Doc::text(","));
    inner.push(Doc::line());
    if let Some(r) = r {
        inner.push(r);
    }
    Doc::group(Doc::concat(vec![
        Doc::text("("),
        Doc::indent(Doc::concat(inner)),
        Doc::softline(),
        Doc::text(")"),
    ]))
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
    let inits: Vec<Node<'_>> = node
        .named_children(&mut walker)
        .filter(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
        .collect();
    if inits.is_empty() {
        return Doc::text("{}");
    }
    let trailing = has_trailing_comma(node);
    let mut inner = Vec::new();
    // A `Doc::hardline()` anywhere inside the Group makes its flat
    // fit-check return false, forcing broken mode unconditionally. We
    // place it where the first `Doc::line()` would have gone — the
    // first child after `{` — so the layout matches normal broken
    // rendering. See [`has_trailing_comma`] for the source-driven
    // opt-in semantics.
    inner.push(if trailing {
        Doc::hardline()
    } else {
        Doc::line()
    });
    for (i, e) in inits.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        // When the initializer is itself an `object_expr` (the
        // `node<T> { Foo {...} }` shape), lower it with the inner
        // `object_fields` *not* wrapped in `Doc::expand`, so the OUTER
        // `object_initializers` group's fit-check counts the inner's
        // full width and breaks at the outer `{` first. Other init
        // expression shapes go through the standard dispatch.
        if e.kind() == "object_expr" {
            inner.push(lower_object_expr_compact_fields(cx, *e));
        } else {
            inner.push(lower_node(cx, *e));
        }
    }
    if trailing {
        // Preserve the source's trailing-comma hint so the formatter
        // is idempotent: re-formatting the broken output still detects
        // the trailing comma and keeps the layout.
        inner.push(Doc::text(","));
    }
    // Outer wrapper keeps `Doc::expand` so that ANY enclosing Group
    // (call args, method chain, etc.) treats the entire `node<T> { … }`
    // shape as zero-width during its own fit-check. The break decision
    // is the outer Group's alone.
    Doc::expand(Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent_if_broken(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ])))
}

/// `object_expr` variant used only from inside [`lower_object_initializers`].
/// Mirrors [`lower_object_expr`] but routes a nested `object_fields` body
/// through [`lower_object_fields_no_expand`] so the inner block's flat
/// width is visible to the enclosing `object_initializers` group. The
/// standalone path (`foo(Foo { … })`, `var x = Foo { … }`) still uses
/// the Expand-wrapped `lower_object_fields` to preserve the "outer chain
/// / call-args group stays flat while only the inner block breaks"
/// behavior.
fn lower_object_expr_compact_fields<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut parts = Vec::new();
    if let Some(t) = node.child_by_field_name("type") {
        parts.push(lower_node(cx, t));
        parts.push(Doc::space());
    }
    let mut walker = node.walk();
    for c in node.named_children(&mut walker) {
        match c.kind() {
            "object_initializers" => parts.push(lower_node(cx, c)),
            "object_fields" => parts.push(lower_object_fields_no_expand(cx, c)),
            _ => {}
        }
    }
    Doc::concat(parts)
}

fn lower_object_fields<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let fields: Vec<Node<'_>> = node
        .named_children(&mut walker)
        .filter(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
        .collect();
    if fields.is_empty() {
        return Doc::text("{}");
    }
    let trailing = has_trailing_comma(node);
    let mut inner = Vec::new();
    inner.push(if trailing {
        Doc::hardline()
    } else {
        Doc::line()
    });
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *f));
    }
    if trailing {
        inner.push(Doc::text(","));
    }
    Doc::expand(Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent_if_broken(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ])))
}

/// `object_fields` variant without the outer `Doc::expand` wrap. Used
/// only from [`lower_object_expr_compact_fields`] — i.e. when the fields
/// block sits directly inside an `object_initializers` and the outer
/// wrapper is the single break point for the combined shape.
fn lower_object_fields_no_expand<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut walker = node.walk();
    let fields: Vec<Node<'_>> = node
        .named_children(&mut walker)
        .filter(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
        .collect();
    if fields.is_empty() {
        return Doc::text("{}");
    }
    let trailing = has_trailing_comma(node);
    let mut inner = Vec::new();
    inner.push(if trailing {
        Doc::hardline()
    } else {
        Doc::line()
    });
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            inner.push(Doc::text(","));
            inner.push(Doc::line());
        }
        inner.push(lower_node(cx, *f));
    }
    if trailing {
        inner.push(Doc::text(","));
    }
    Doc::group(Doc::concat(vec![
        Doc::text("{"),
        Doc::indent_if_broken(Doc::concat(inner)),
        Doc::line(),
        Doc::text("}"),
    ]))
}

/// Detect a source-level trailing comma between the last item and the
/// closing brace. Walks the node's anonymous children (separators) and
/// returns `true` iff the *last* non-comment child is preceded by a `,`
/// before the `}`. Block / line comments between the last item and the
/// closing brace are ignored — they don't change the trailing-comma
/// status. Used by [`lower_object_fields`], [`lower_object_fields_no_expand`]
/// and [`lower_object_initializers`] to support a "trailing comma forces
/// multi-line" opt-in mirroring rustfmt / Black.
fn has_trailing_comma(node: Node<'_>) -> bool {
    let mut walker = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut walker).collect();
    let Some(last_item_idx) = children
        .iter()
        .rposition(|c| c.is_named() && !matches!(c.kind(), "block_comment" | "line_comment"))
    else {
        return false;
    };
    children[last_item_idx + 1..]
        .iter()
        .any(|c| !c.is_named() && c.kind() == ",")
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
    let elems: Vec<Node<'_>> = node
        .named_children(&mut walker)
        .filter(|c| !matches!(c.kind(), "block_comment" | "line_comment"))
        .collect();
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
    Doc::expand(Doc::group(Doc::concat(vec![
        Doc::text("["),
        Doc::indent_if_broken(Doc::concat(inner)),
        Doc::softline(),
        Doc::text("]"),
    ])))
}

fn lower_call_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_chain_root(cx, node)
}

fn lower_member_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_chain_root(cx, node)
}

fn lower_arrow_expr<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    lower_chain_root(cx, node)
}

/// Postfix chain link. Each link contributes content after the head
/// (or previous link) in source order. `leading` is glued to whatever
/// precedes it (no break-point); `after_break`, when present, is the
/// portion that can flow onto a continuation line when the chain Group
/// breaks. Break-points sit between `leading` and `after_break`.
struct ChainLink {
    leading: Doc,
    after_break: Option<Doc>,
}

/// Lower an outermost postfix-chain node (`member_expr`, `arrow_expr`,
/// `call_expr`, `offset_expr`) into one Group so the renderer can break
/// the chain at `.` / `->` points when it overflows. Trivial chains
/// (single call / single member access) collapse to flat text via the
/// Group's fits check.
fn lower_chain_root<'a>(cx: &Cx<'a>, node: Node<'a>) -> Doc {
    let mut head: Option<Doc> = None;
    let mut links: Vec<ChainLink> = Vec::new();
    collect_postfix_chain(cx, node, &mut head, &mut links);
    let head_doc = head.unwrap_or(Doc::nil());

    if links.is_empty() {
        return head_doc;
    }

    // Split the link content into a head-glued prefix (pre-softline)
    // and a chain-indented tail (post-softline). The tail goes inside
    // an IndentIfBroken so the chain's indent step only fires when the
    // chain itself breaks — leaving inner expandable groups (args /
    // object inits / arrays) to drive their own indent on their own
    // break decision when the chain stays flat.
    let mut prefix: Vec<Doc> = Vec::new();
    let mut tail: Vec<Doc> = Vec::new();
    let mut crossed_softline = false;
    for link in links {
        if !is_doc_nil(&link.leading) {
            if crossed_softline {
                tail.push(link.leading);
            } else {
                prefix.push(link.leading);
            }
        }
        if let Some(trail) = link.after_break {
            crossed_softline = true;
            tail.push(Doc::softline());
            tail.push(trail);
        }
    }
    let mut result: Vec<Doc> = vec![head_doc];
    result.extend(prefix);
    if !tail.is_empty() {
        result.push(Doc::indent_if_broken(Doc::concat(tail)));
    }
    Doc::group(Doc::concat(result))
}

fn is_doc_nil(d: &Doc) -> bool {
    matches!(d, Doc::Nil)
}

/// Walk down the postfix chain (through `receiver` / `fn`), collecting
/// per-link contributions in source order. Stops descending when the
/// child is no longer a postfix kind — that subtree becomes the head
/// (lowered through `lower_node` to preserve its own structure).
fn collect_postfix_chain<'a>(
    cx: &Cx<'a>,
    node: Node<'a>,
    out_head: &mut Option<Doc>,
    out_links: &mut Vec<ChainLink>,
) {
    match node.kind() {
        "member_expr" => collect_member_link(cx, node, ".", out_head, out_links),
        "arrow_expr" => collect_member_link(cx, node, "->", out_head, out_links),
        "call_expr" => {
            if let Some(fn_node) = node.child_by_field_name("fn") {
                collect_postfix_chain(cx, fn_node, out_head, out_links);
            }
            let mut walker = node.walk();
            for c in node.named_children(&mut walker) {
                if c.kind() == "args" {
                    out_links.push(ChainLink {
                        leading: lower_args_call(cx, c),
                        after_break: None,
                    });
                    break;
                }
            }
        }
        "offset_expr" => collect_offset_link(cx, node, out_head, out_links),
        _ => *out_head = Some(lower_node(cx, node)),
    }
}

/// Build a member-or-arrow link: `[pre_?]<sep><prop>[post_?]`. The
/// break-point sits before `<sep>`, so the pre-optional `?` glues to
/// the previous link / head and travels on the leading side.
fn collect_member_link<'a>(
    cx: &Cx<'a>,
    node: Node<'a>,
    sep: &'static str,
    out_head: &mut Option<Doc>,
    out_links: &mut Vec<ChainLink>,
) {
    let mut walker = node.walk();
    let mut emitted_sep = false;
    let mut recv: Option<Node<'_>> = None;
    let mut prop_doc: Option<Doc> = None;
    let mut pre_opt = false;
    let mut post_opt = false;

    for c in node.children(&mut walker) {
        if !c.is_named() {
            if !c.byte_range().is_empty() {
                emitted_sep = true;
            }
            continue;
        }
        if c.kind() == "optional" {
            if emitted_sep {
                post_opt = true;
            } else {
                pre_opt = true;
            }
        } else if !emitted_sep {
            recv = Some(c);
        } else {
            prop_doc = Some(Doc::text(cx.text(c).to_string()));
        }
    }

    if let Some(r) = recv {
        collect_postfix_chain(cx, r, out_head, out_links);
    }

    let leading = if pre_opt { Doc::text("?") } else { Doc::nil() };
    let mut trail_parts = vec![Doc::text(sep.to_string())];
    if let Some(p) = prop_doc {
        trail_parts.push(p);
    }
    if post_opt {
        trail_parts.push(Doc::text("?"));
    }
    out_links.push(ChainLink {
        leading,
        after_break: Some(Doc::concat(trail_parts)),
    });
}

fn collect_offset_link<'a>(
    cx: &Cx<'a>,
    node: Node<'a>,
    out_head: &mut Option<Doc>,
    out_links: &mut Vec<ChainLink>,
) {
    let mut walker = node.walk();
    let mut state: u8 = 0; // 0 = before `[`, 1 = between `[` and `]`, 2 = after `]`
    let mut recv: Option<Node<'_>> = None;
    let mut idx: Option<Node<'_>> = None;
    let mut pre_opt = false;
    let mut post_opt = false;

    for c in node.children(&mut walker) {
        if c.is_named() {
            if c.kind() == "optional" {
                if state == 0 {
                    pre_opt = true;
                } else if state == 2 {
                    post_opt = true;
                }
            } else if state == 0 {
                recv = Some(c);
            } else if state == 1 {
                idx = Some(c);
            }
        } else if !c.byte_range().is_empty() {
            match cx.text(c) {
                "[" => state = 1,
                "]" => state = 2,
                _ => {}
            }
        }
    }

    if let Some(r) = recv {
        collect_postfix_chain(cx, r, out_head, out_links);
    }

    let mut leading_parts: Vec<Doc> = Vec::new();
    if pre_opt {
        leading_parts.push(Doc::text("?"));
    }
    leading_parts.push(Doc::text("["));
    if let Some(i) = idx {
        leading_parts.push(lower_node(cx, i));
    }
    leading_parts.push(Doc::text("]"));
    if post_opt {
        leading_parts.push(Doc::text("?"));
    }
    out_links.push(ChainLink {
        leading: Doc::concat(leading_parts),
        after_break: None,
    });
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
    lower_chain_root(cx, node)
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
