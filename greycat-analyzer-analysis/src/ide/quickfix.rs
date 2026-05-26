//! Per-diagnostic auto-fix synthesis.
//!
//! Single source of truth for "given a diagnostic on this source, what
//! edits would make it go away?" — consumed by both the CLI's
//! `lint --fix` driver and the LSP's `textDocument/codeAction` handler.
//! These previously lived as parallel implementations in each
//! caller; the duplication was the dominant source of "fix in one
//! place, forget the other" bugs.
//!
//! The fix functions are byte-range based. Callers that work in LSP
//! `Position` space convert at the boundary — that conversion is not
//! the quickfix module's concern.
//!
//! All fix functions return `Vec<TextEdit>`; an empty Vec means "this
//! diagnostic has no automatic fix" (or its preconditions don't hold).
//!
//! Fix functions trust the lint emission. There is no independent
//! re-verification ("is this name *really* unused?") inside the fix —
//! the parse-safety nets in the LSP `code_actions` handler and the
//! CLI `lint --fix` driver re-parse the result and reject edits that
//! introduce new parse errors, which is the load-bearing downstream
//! guard. If a lint is producing wrong diagnostics, the fix is at the
//! lint, not at the consumer.

use std::ops::Range;

use super::actions::TextEdit;
use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_syntax::tree_sitter::Node;

/// Shared inputs every quickfix may need. Bundled so the public entry
/// signature doesn't grow per added analysis dependency.
///
/// `hir` / `symbols` are optional so quickfix unit tests and callers
/// that only have CST + text in hand (no full pipeline) still work —
/// rules that genuinely need scope information (currently only
/// `unused-local`'s rename-on-impure-init path) check `Option::is_some`
/// and degrade gracefully when absent. Production callers (the LSP
/// `textDocument/codeAction` handler and the CLI `lint --fix` driver)
/// always populate them; both have a cached `ModuleAnalysis` in hand.
pub struct QuickfixCx<'a> {
    pub root: Node<'a>,
    pub text: &'a str,
    pub hir: Option<&'a Hir>,
    pub symbols: Option<&'a SymbolTable>,
}

impl<'a> QuickfixCx<'a> {
    /// CST-only context. Use when no `ModuleAnalysis` is available.
    /// Rules that need scope information fall back to a safe default.
    pub fn from_cst(root: Node<'a>, text: &'a str) -> Self {
        Self {
            root,
            text,
            hir: None,
            symbols: None,
        }
    }
}

/// Compute the auto-fix edits for `diag` against `cx`. Returns an
/// empty Vec when the rule has no fix or its preconditions don't hold.
pub fn edit_for_diagnostic(
    cx: &QuickfixCx<'_>,
    code: &str,
    byte_range: &Range<usize>,
    message: &str,
) -> Vec<TextEdit> {
    let start = byte_range.start;
    let end = byte_range.end;
    if end > cx.text.len() || start > end {
        return Vec::new();
    }
    match code {
        "missing-token" => missing_token_fix(start, message),
        "unused-local" => unused_local_fix(cx, start, end),
        "unused-decl" => unused_decl_fix(cx.root, start),
        "unused-param" => unused_param_fix(cx.text, start, end),
        "unused-generic-param" => unused_generic_param_fix(cx.root, cx.text, start, end),
        "possibly-null" => possibly_null_fix(cx.text, end),
        "redundant-nullable-access" => redundant_nullable_access_fix(cx.text, start, end),
        "redundant-non-null-assertion" | "redundant-coalesce" => redundant_slice_fix(start, end),
        "modvar-node-cannot-be-nullable" => modvar_strip_outer_nullable_fix(cx.text, end),
        "modvar-node-inner-must-be-nullable" => modvar_append_inner_nullable_fix(end),
        "unused-suppression" => unused_suppression_fix(cx.root, cx.text, start, end),
        "empty-suppression" | "unbalanced-lint-off" | "unbalanced-fmt-off" => {
            delete_comment_line_fix(cx.root, cx.text, start)
        }
        "unreachable" => unreachable_fix(cx.root, cx.text, start, end),
        "non-exhaustive" => non_exhaustive_fix(cx.root, cx.text, start, message),
        "catch-empty-parens" => catch_empty_parens_fix(cx.text, start, end),
        "unused-catch-param" => unused_catch_param_fix(cx.root, cx.text, start),
        "redundant-semicolon" => redundant_semicolon_fix(start, end),
        "no-breakpoint" => no_breakpoint_fix(cx.text, start, end),
        "infer-return-type" => infer_return_type_fix(cx.root, start, message),
        "private-cross-module-name" => private_cross_module_fix(start, end, message),
        _ => Vec::new(),
    }
}

/// Rewrite a bare ident that referenced a `private` cross-module decl
/// to the suggested `module::Name` FQN. The analyzer's diagnostic
/// message always ends with `` … use `<module>::<Name>` ``, so the
/// last backtick pair carries the replacement string verbatim.
fn private_cross_module_fix(start: usize, end: usize, message: &str) -> Vec<TextEdit> {
    if end <= start {
        return Vec::new();
    }
    let Some(last_close) = message.rfind('`') else {
        return Vec::new();
    };
    let Some(last_open) = message[..last_close].rfind('`') else {
        return Vec::new();
    };
    let fqn = &message[last_open + 1..last_close];
    if !fqn.contains("::") {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: start..end,
        new_text: fqn.to_owned(),
    }]
}

/// Insert `: T` between the parameter list's closing `)` and the body's
/// opening `{` of the fn whose name span the diagnostic points at.
/// The target type comes from the diagnostic message — the lint
/// embeds it between backticks (`return type can be inferred as
/// `T``). Pulling it from the message keeps the fix decoupled from
/// the type system: no second walk of HIR / arena / decl registry, and
/// every type the lint chose to surface is by construction a string
/// the user can paste into source.
///
/// No-op when:
/// - The enclosing fn / method node can't be located (defensive).
/// - The fn already has a `return_type` field (the lint shouldn't fire
///   on those, but a stale diagnostic from before an edit might).
/// - The message doesn't carry a backtick-quoted type name.
fn infer_return_type_fix(root: Node<'_>, ident_start: usize, message: &str) -> Vec<TextEdit> {
    let Some(ty) = extract_backtick_quoted(message) else {
        return Vec::new();
    };
    let mut node = match root.descendant_for_byte_range(ident_start, ident_start) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let fn_node = loop {
        if matches!(node.kind(), "fn_decl" | "type_method") {
            break node;
        }
        node = match node.parent() {
            Some(p) => p,
            None => return Vec::new(),
        };
    };
    if fn_node.child_by_field_name("return_type").is_some() {
        return Vec::new();
    }
    let Some(params) = fn_node.child_by_field_name("params") else {
        return Vec::new();
    };
    let insert_at = params.end_byte();
    vec![TextEdit {
        byte_range: insert_at..insert_at,
        new_text: format!(": {ty}"),
    }]
}

/// Extract the substring between the first matching pair of backticks
/// in `s`, or `None` when no such pair exists. Used by the
/// `infer-return-type` fix to recover the suggested type from the
/// diagnostic message without re-running the inference.
fn extract_backtick_quoted(s: &str) -> Option<&str> {
    let start = s.find('`')? + 1;
    let end_offset = s[start..].find('`')?;
    Some(&s[start..start + end_offset])
}

// P37.7
/// Delete a `breakpoint;` statement. The diagnostic's byte range covers
/// exactly the `breakpoint_stmt` CST node (`breakpoint;`). If the stmt
/// occupies a line on its own (only leading whitespace before, only `\n`
/// after) the fix also eats the leading indent + trailing newline so
/// no blank line is left behind. Same shape as `unreachable_fix`'s
/// trailing-line cleanup.
fn no_breakpoint_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let bytes = text.as_bytes();
    if end > bytes.len() || start > end {
        return Vec::new();
    }
    let mut del_start = start;
    let mut del_end = end;
    if del_end < bytes.len() && bytes[del_end] == b'\n' {
        let line_start = text[..del_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let pre_only_ws = text[line_start..del_start]
            .chars()
            .all(|c| c.is_whitespace());
        if pre_only_ws {
            del_end += 1;
            del_start = line_start;
        }
    }
    vec![TextEdit {
        byte_range: del_start..del_end,
        new_text: String::new(),
    }]
}

/// Delete the stray trailing `;` (or `;;;`) after a fn / method body.
/// The diagnostic's byte range already covers exactly the
/// `block_trailing_semi` CST node, so the fix is just a slice-delete.
fn redundant_semicolon_fix(start: usize, end: usize) -> Vec<TextEdit> {
    if end <= start {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: start..end,
        new_text: String::new(),
    }]
}

/// Delete the `(e)` after a `catch` keyword, including the leading
/// whitespace, so `catch (e) { … }` becomes `catch { … }`. Locates the
/// enclosing `try_stmt` and walks its anonymous-token children for the
/// `(` and `)` that bracket the `error_param` ident.
fn unused_catch_param_fix(root: Node<'_>, text: &str, ident_start: usize) -> Vec<TextEdit> {
    let Some(node) = enclosing_kind(root, ident_start, "try_stmt") else {
        return Vec::new();
    };
    let mut cur = node.walk();
    let mut open: Option<usize> = None;
    let mut close: Option<usize> = None;
    for ch in node.children(&mut cur) {
        if ch.is_named() {
            continue;
        }
        let s = &text[ch.byte_range()];
        if s == "(" && open.is_none() {
            open = Some(ch.start_byte());
        } else if s == ")" && open.is_some() {
            close = Some(ch.end_byte());
        }
    }
    let (Some(start), Some(end)) = (open, close) else {
        return Vec::new();
    };
    let mut new_start = start;
    let bytes = text.as_bytes();
    while new_start > 0 {
        let b = bytes[new_start - 1];
        if b == b' ' || b == b'\t' {
            new_start -= 1;
        } else {
            break;
        }
    }
    vec![TextEdit {
        byte_range: new_start..end,
        new_text: String::new(),
    }]
}

/// Delete the empty `(...)` plus any whitespace immediately preceding it
/// (typically the single space after `catch`), so `catch () { … }` becomes
/// `catch { … }`. The diagnostic's range already covers the parens; we
/// extend left to absorb the leading whitespace so the result has tidy
/// spacing.
fn catch_empty_parens_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    if end <= start || end > text.len() {
        return Vec::new();
    }
    let mut new_start = start;
    let bytes = text.as_bytes();
    while new_start > 0 {
        let b = bytes[new_start - 1];
        if b == b' ' || b == b'\t' {
            new_start -= 1;
        } else {
            break;
        }
    }
    vec![TextEdit {
        byte_range: new_start..end,
        new_text: String::new(),
    }]
}

// =============================================================================
// Per-rule fix construction
// =============================================================================

fn missing_token_fix(start: usize, message: &str) -> Vec<TextEdit> {
    let Some(token) = message
        .split_once('`')
        .and_then(|(_, rest)| rest.split_once('`').map(|(t, _)| t))
    else {
        return Vec::new();
    };
    vec![TextEdit {
        byte_range: start..start,
        new_text: token.to_string(),
    }]
}

// P22.1
/// Pick a safe quickfix for `unused-local`:
///
/// - For for-init (`for (var i = …; …)`) and for-in (`for (k, v in …)`)
///   binders, the ident sits in a structural slot — the binding can't
///   be removed without breaking the loop — so rename to `_name`,
///   matching `unused-param`'s shape.
/// - For a plain `var x = expr;` with **no initializer** or an
///   initializer whose RHS is provably side-effect-free, delete the
///   whole statement.
/// - For a plain `var x = expr;` whose RHS **may have side effects**
///   (calls, mutating ops, throwing checks, container subscripts, or
///   any kind we don't recognize), rename the binder to `_name` so
///   the initializer (and its side effects) still execute. When
///   `_name` is already taken in the local scope, escalate to
///   `_name_2`, `_name_3`, … — the collision set comes from the
///   shared `ide::scope::names_in_scope_at`, which mirrors the
///   resolver's lexical scoping.
fn unused_local_fix(cx: &QuickfixCx<'_>, ident_start: usize, ident_end: usize) -> Vec<TextEdit> {
    // for-init / for-in: rename (the bind is structurally required).
    if enclosing_node_range(cx.root, ident_start, &["for_stmt", "for_in_stmt"]).is_some() {
        return unused_param_fix(cx.text, ident_start, ident_end);
    }
    let Some(var_decl) = enclosing_kind(cx.root, ident_start, "var_decl") else {
        return Vec::new();
    };
    let mut cursor = var_decl.walk();
    let init_expr = var_decl
        .named_children(&mut cursor)
        .find(|c| c.kind() == "initializer")
        .and_then(|init| init.child_by_field_name("expr"));
    let safe_to_delete = match init_expr {
        None => true,
        Some(expr) => is_side_effect_free(expr, cx.text),
    };
    if safe_to_delete {
        return vec![TextEdit {
            byte_range: var_decl.byte_range(),
            new_text: String::new(),
        }];
    }
    rename_to_fresh_underscore(cx, ident_start, ident_end)
}

/// Conservative purity classifier for an expression CST node. Returns
/// `true` only when the expression provably cannot:
///
/// - call user code (fn / method / `++` / `--` operators that mutate a
///   place, `!!` runtime non-null assertion that may throw),
/// - mutate observable state (`=` / `?=` binary operators),
/// - alter control flow via a runtime throw (out-of-bounds subscript).
///
/// Unknown node kinds default to `false` (impure) — under-deletion is
/// a benign worsening of the fix; over-deletion silently eats side
/// effects the user did not consent to.
fn is_side_effect_free(node: Node<'_>, text: &str) -> bool {
    match node.kind() {
        // Pure terminals.
        "string" | "number" | "char" | "true" | "false" | "null" | "this" | "ident" => true,
        // Static path — name lookup, no call.
        "static_expr" => true,
        // Closure value. Calling it would be impure; constructing it
        // is not.
        "lambda_expr" => true,
        // Recurse on the single inner expr.
        "paren_expr" => node
            .child_by_field_name("expr")
            .map(|inner| is_side_effect_free(inner, text))
            .unwrap_or(false),
        // Member / arrow read — recurse on the receiver. The property
        // name itself isn't an expression.
        "member_expr" | "arrow_expr" => {
            named_child_exprs(node).all(|child| is_side_effect_free(child, text))
        }
        // Two-operand exprs whose operator decides purity. `=` / `?=`
        // are mutating; `as` / `is` are pure type checks (right
        // operand is a `type_ident`, not an `_expr`).
        "binary_expr" => {
            binary_op_is_pure(node, text)
                && named_child_exprs(node).all(|child| is_side_effect_free(child, text))
        }
        // Prefix `++` / `--` mutate the operand; postfix `++` / `--`
        // do too; postfix `!!` throws on null. All other unary ops
        // (`-`, `!`, `+`, `*`) are pure when the operand is.
        "unary_expr" => {
            unary_op_is_pure(node, text)
                && named_child_exprs(node).all(|child| is_side_effect_free(child, text))
        }
        // Container constructors — pure when every element is.
        "tuple_expr" | "array_expr" | "range_expr" | "interval_expr" => {
            named_child_exprs(node).all(|child| is_side_effect_free(child, text))
        }
        // Object literal: `Foo { name: value, … }`. Recurse on every
        // `object_field`'s `value` (the `name` slot can carry a
        // computed key but is conventionally an ident — recurse
        // anyway). `object_initializers` (positional) wraps `_expr`s
        // directly.
        "object_expr" => {
            let mut cursor = node.walk();
            node.children(&mut cursor).all(|child| match child.kind() {
                "object_initializers" => {
                    let mut c2 = child.walk();
                    child
                        .named_children(&mut c2)
                        .all(|e| is_side_effect_free(e, text))
                }
                "object_fields" => {
                    let mut c2 = child.walk();
                    child.named_children(&mut c2).all(|field| {
                        let name_pure = field
                            .child_by_field_name("name")
                            .map(|n| is_side_effect_free(n, text))
                            .unwrap_or(true);
                        let value_pure = field
                            .child_by_field_name("value")
                            .map(|v| is_side_effect_free(v, text))
                            .unwrap_or(true);
                        name_pure && value_pure
                    })
                }
                _ => true,
            })
        }
        // Calls, subscripts, anything else: impure (or unknown — same
        // treatment).
        _ => false,
    }
}

/// True when `binary_expr`'s operator is one of the pure ones. Reads
/// the operator from the source between `left` and `right`.
fn binary_op_is_pure(node: Node<'_>, text: &str) -> bool {
    let (Some(left), Some(right)) = (
        node.child_by_field_name("left"),
        node.child_by_field_name("right"),
    ) else {
        return false;
    };
    let between = text[left.end_byte()..right.start_byte()].trim();
    // `=` / `?=` are the only mutating binary operators in GreyCat.
    // Every other op is value-pure (`as` / `is` included — they're
    // type checks that don't run user code).
    !matches!(between, "=" | "?=")
}

/// True when `unary_expr`'s operator is one of the pure ones. Mirrors
/// `binary_op_is_pure` — reads the operator slice around the operand.
fn unary_op_is_pure(node: Node<'_>, text: &str) -> bool {
    let mut cursor = node.walk();
    let operand = node
        .named_children(&mut cursor)
        .next()
        .map(|c| c.byte_range());
    let Some(operand) = operand else {
        return false;
    };
    let leading = text[node.start_byte()..operand.start].trim();
    let trailing = text[operand.end..node.end_byte()].trim();
    // Mutating: `++` / `--` either side. Throwing: postfix `!!`.
    // Everything else (`-`, `!`, `+`, `*`) is pure on a pure operand.
    !matches!(leading, "++" | "--") && !matches!(trailing, "++" | "--" | "!!")
}

/// Iterator over a node's named children that are themselves
/// expressions — i.e. anything that could be passed to
/// `is_side_effect_free`. Filters out `type_ident`-shaped children
/// (the right operand of `as` / `is`) and the property slot of
/// `member_expr` / `arrow_expr` (an `ident` is pure on its own).
fn named_child_exprs<'tree>(node: Node<'tree>) -> impl Iterator<Item = Node<'tree>> {
    let mut cursor = node.walk();
    let children: Vec<Node<'tree>> = node
        .named_children(&mut cursor)
        .filter(|c| !matches!(c.kind(), "type_ident" | "optional"))
        .collect();
    children.into_iter()
}

/// Rename the binder at `ident_start..ident_end` to the first
/// `_name`-shaped candidate that doesn't collide with any name
/// visible in the local scope at that position. Probes `_<name>`,
/// `_<name>_2`, `_<name>_3`, … in order.
///
/// When no HIR is available (CST-only context), falls back to a plain
/// `_<name>` rename — accepts the (unlikely) risk of shadowing
/// because the alternative (refusing the fix entirely) is worse for
/// the unit-test path.
fn rename_to_fresh_underscore(
    cx: &QuickfixCx<'_>,
    ident_start: usize,
    ident_end: usize,
) -> Vec<TextEdit> {
    if ident_end <= ident_start {
        return Vec::new();
    }
    let name = &cx.text[ident_start..ident_end];
    if name.starts_with('_') {
        return Vec::new();
    }
    let fresh = match (cx.hir, cx.symbols) {
        (Some(hir), Some(symbols)) => {
            let taken = crate::ide::scope::names_in_scope_at(hir, symbols, ident_start);
            let mut candidate = format!("_{name}");
            let mut n: u32 = 2;
            loop {
                let candidate_sym = symbols.lookup(&candidate);
                let collision = candidate_sym
                    .map(|sym| taken.contains(&sym))
                    .unwrap_or(false);
                if !collision {
                    break;
                }
                candidate = format!("_{name}_{n}");
                n += 1;
            }
            candidate
        }
        _ => format!("_{name}"),
    };
    vec![TextEdit {
        byte_range: ident_start..ident_end,
        new_text: fresh,
    }]
}

// P22.2
/// Same shape for top-level decls. Walks to the enclosing
/// `fn_decl` / `type_decl` / `enum_decl` / `modvar` and returns its
/// full byte range. Doc comments + annotations sitting immediately
/// above the decl are absorbed (the grammar makes them children of
/// the decl, so the decl's `byte_range` already covers them).
fn unused_decl_fix(root: Node<'_>, ident_start: usize) -> Vec<TextEdit> {
    let Some(decl_range) = enclosing_node_range(
        root,
        ident_start,
        &["fn_decl", "type_decl", "enum_decl", "modvar"],
    ) else {
        return Vec::new();
    };
    vec![TextEdit {
        byte_range: decl_range,
        new_text: String::new(),
    }]
}

// P22.3
/// Rename `name` to `_name`. The lint has already established that
/// the body doesn't reference `name`; if it ever produces a false
/// positive, the parse-safety net upstream catches edits that break
/// the parse. The `_`-prefix early-out is part of the rule's own
/// vocabulary (`_x` is the project convention for "intentionally
/// unused"), not an independent safety check.
fn unused_param_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    if end <= start {
        return Vec::new();
    }
    let name = &text[start..end];
    if name.starts_with('_') {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: start..end,
        new_text: format!("_{name}"),
    }]
}

/// Strip an unused generic parameter from its enclosing `<...>` by
/// walking the `type_params` node's children directly:
///
/// - Sole param (`<T>`): cut the whole `type_params` block.
/// - First of many (`<T, X>` → `<X>`): cut from ident start through the
///   following `,` plus any whitespace before the next ident.
/// - Not first (`<X, T>` / `<X, T, Y>`): cut from the preceding `,`
///   through the ident's end.
///
/// Trusts the lint; `_`-prefixed names are skipped because that prefix
/// is the convention for "intentionally unused."
fn unused_generic_param_fix(
    root: Node<'_>,
    text: &str,
    ident_start: usize,
    ident_end: usize,
) -> Vec<TextEdit> {
    if ident_end <= ident_start || ident_end > text.len() {
        return Vec::new();
    }
    let name = &text[ident_start..ident_end];
    if name.starts_with('_') {
        return Vec::new();
    }
    // Find the enclosing `type_params` node (parent of the binding ident).
    let Some(params) = enclosing_kind(root, ident_start, "type_params") else {
        return Vec::new();
    };
    let mut cursor = params.walk();
    let named_idents: Vec<Node<'_>> = params
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "ident")
        .collect();
    let Some(idx) = named_idents
        .iter()
        .position(|n| n.start_byte() == ident_start && n.end_byte() == ident_end)
    else {
        return Vec::new();
    };
    // Sole param — drop the whole `<T>` block.
    if named_idents.len() == 1 {
        return vec![TextEdit {
            byte_range: params.byte_range(),
            new_text: String::new(),
        }];
    }
    // Find the comma adjacent to this param: previous comma if `idx > 0`,
    // otherwise the following comma.
    let bytes = text.as_bytes();
    let (cut_start, cut_end) = if idx > 0 {
        // Walk backwards from ident_start across whitespace; expect a `,`.
        let mut i = ident_start;
        while i > params.start_byte() && matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r') {
            i -= 1;
        }
        if i == 0 || bytes[i - 1] != b',' {
            return Vec::new();
        }
        (i - 1, ident_end)
    } else {
        // First of many: walk forwards from ident_end across whitespace;
        // expect a `,`, then eat trailing whitespace before the survivor.
        let mut i = ident_end;
        while i < params.end_byte() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b',' {
            return Vec::new();
        }
        let mut after = i + 1;
        while after < params.end_byte() && matches!(bytes[after], b' ' | b'\t') {
            after += 1;
        }
        (ident_start, after)
    };
    vec![TextEdit {
        byte_range: cut_start..cut_end,
        new_text: String::new(),
    }]
}

fn possibly_null_fix(text: &str, recv_end: usize) -> Vec<TextEdit> {
    let bytes = text.as_bytes();
    let mut i = recv_end;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    let is_op = bytes
        .get(i)
        .map(|b| matches!(b, b'.' | b'[' | b'?'))
        .unwrap_or(false)
        || (bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'>'));
    if !is_op || bytes.get(i) == Some(&b'?') {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: i..i,
        new_text: "?".into(),
    }]
}

fn redundant_nullable_access_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let bytes = text.as_bytes();
    let Some(q) = bytes[start..end]
        .iter()
        .position(|b| *b == b'?')
        .map(|off| start + off)
    else {
        return Vec::new();
    };
    vec![TextEdit {
        byte_range: q..q + 1,
        new_text: String::new(),
    }]
}

fn redundant_slice_fix(start: usize, end: usize) -> Vec<TextEdit> {
    if end <= start {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: start..end,
        new_text: String::new(),
    }]
}

fn modvar_strip_outer_nullable_fix(text: &str, end: usize) -> Vec<TextEdit> {
    if end == 0 || text.as_bytes().get(end - 1) != Some(&b'?') {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: (end - 1)..end,
        new_text: String::new(),
    }]
}

fn modvar_append_inner_nullable_fix(end: usize) -> Vec<TextEdit> {
    vec![TextEdit {
        byte_range: end..end,
        new_text: "?".into(),
    }]
}

// =============================================================================
// Helpers
// =============================================================================

/// Re-parse `text` and walk from the byte position to the smallest
/// enclosing node whose `kind()` is in `kinds`. Returns the node's full
/// `byte_range`, or `None` if no such ancestor exists or the parse
/// fails. The re-parse is local to this call — no caching, no shared
/// state. Re-parsing a single file is on the order of microseconds, so
/// the simplicity wins.
// P24.6
/// Fix for `unreachable`. The diagnostic's byte range is
/// already the dead island (single statement, coalesced sibling run,
/// or trailing `else { … }` block). Default: delete that range.
///
/// Special case for the dead-`else` shape: when the dead range starts
/// at a `{` (the body of a final else under exhaustive coverage), walk
/// back over whitespace to find and swallow the leading `else` keyword
/// alongside any whitespace between them. Otherwise we'd leave
/// `if (…) { … } else ` dangling — a parse error.
fn unreachable_fix(root: Node<'_>, text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let bytes = text.as_bytes();
    if end > bytes.len() || start > end {
        return Vec::new();
    }
    // Detect the trivially-decidable `if` shape: range starts at the
    // `if` keyword. When the if has an `else` branch, unwrap to its
    // contents (the else block is the live branch); otherwise plain
    // delete handles it. The condition's truth value isn't passed
    // through here — we walk to the enclosing if_stmt to inspect its
    // structure.
    if range_starts_with_keyword(bytes, start, b"if")
        && let Some(edit) = trivially_decidable_if_fix(root, text, start)
    {
        return vec![edit];
    }
    let mut del_start = start;
    // Detect the "dead else block" shape: the diagnostic range starts
    // at a `{` whose enclosing block is the else-branch of an `if_stmt`.
    // Walk up via the CST to find the `else` token and swallow it (plus
    // its leading whitespace).
    if bytes.get(start) == Some(&b'{')
        && let Some(else_kw_start) = enclosing_else_token_start(root, start)
    {
        // Stop at a newline so the prior block's indentation isn't disturbed.
        let mut j = else_kw_start;
        while j > 0 && matches!(bytes[j - 1], b' ' | b'\t') {
            j -= 1;
        }
        del_start = j;
    }
    // Eat a trailing newline if the deletion would otherwise leave a
    // blank line behind (the dead range was the only content on its
    // line(s)). Cheap heuristic: when the byte after `end` is `\n`
    // and the line preceding `del_start` looks empty.
    let mut del_end = end;
    if del_end < bytes.len() && bytes[del_end] == b'\n' {
        let line_start = text[..del_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let pre_only_ws = text[line_start..del_start]
            .chars()
            .all(|c| c.is_whitespace());
        if pre_only_ws {
            del_end += 1;
            del_start = line_start;
        }
    }
    vec![TextEdit {
        byte_range: del_start..del_end,
        new_text: String::new(),
    }]
}

/// Whether `bytes[start..]` begins with `kw` followed by a non-word
/// byte (or EOF). Used by `unreachable_fix` to distinguish ranges
/// that start at an `if` / `while` / `for` keyword from ranges that
/// happen to contain those letters as part of an identifier.
fn range_starts_with_keyword(bytes: &[u8], start: usize, kw: &[u8]) -> bool {
    if start + kw.len() > bytes.len() {
        return false;
    }
    if &bytes[start..start + kw.len()] != kw {
        return false;
    }
    match bytes.get(start + kw.len()) {
        None => true,
        Some(b) => !matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'),
    }
}

/// Quickfix shape for a trivially-decidable `if` whose dead range
/// starts at the `if` keyword. Walks `root` for the enclosing
/// `if_stmt`; returns `Some(edit)` when the if has an `else_branch`
/// (we unwrap to its contents) and `None` when there is no else (the
/// caller falls back to plain delete).
fn trivially_decidable_if_fix(root: Node<'_>, text: &str, start: usize) -> Option<TextEdit> {
    let mut node = root.descendant_for_byte_range(start, start + 1)?;
    while node.kind() != "if_stmt" {
        node = node.parent()?;
    }
    let else_branch = find_else_branch(node)?;
    let if_range = node.byte_range();
    let new_text = match else_branch.kind() {
        "block" => unwrap_block_to_outer_indent(text, if_range.start, else_branch.byte_range()),
        // `else if (…) { … }` → keep the nested if-stmt verbatim.
        _ => text[else_branch.byte_range()].to_string(),
    };
    Some(TextEdit {
        byte_range: if_range,
        new_text,
    })
}

/// If `block_start` is the start byte of a `block` node that is the
/// else-branch payload of an `if_stmt`, return the start byte of the
/// `else` keyword token sitting between the then-branch and that block.
/// Used by `unreachable_fix` to delete the `else` along with its dead
/// block body. Returns `None` when the block isn't an else-payload.
fn enclosing_else_token_start(root: Node<'_>, block_start: usize) -> Option<usize> {
    // Descend to the smallest node at the `{` byte (usually the `{`
    // anonymous token), then walk up to the enclosing `block` node.
    let mut node = root.descendant_for_byte_range(block_start, block_start)?;
    while node.kind() != "block" {
        node = node.parent()?;
    }
    let block = node;
    let if_stmt = block.parent()?;
    if if_stmt.kind() != "if_stmt" {
        return None;
    }
    // Confirm this block is the else-branch payload, not the then-branch.
    let then_id = if_stmt.child_by_field_name("then_branch")?.id();
    if block.id() == then_id {
        return None;
    }
    // Walk the if_stmt's anonymous children for the `else` token that
    // sits between then_branch and this block. `_else_branch` is an
    // inlined anonymous rule (`seq("else", choice($.if_stmt, $.block))`),
    // so the `else` keyword appears as a direct child of `if_stmt`.
    let mut cursor = if_stmt.walk();
    for ch in if_stmt.children(&mut cursor) {
        if !ch.is_named() && ch.kind() == "else" && ch.end_byte() <= block.start_byte() {
            return Some(ch.start_byte());
        }
    }
    None
}

/// Find the else-branch *payload* of an `if_stmt` — either the
/// `{ … }` block or a nested `if_stmt` (`else if`). The grammar's
/// `else_branch` field points at the literal `else` keyword token,
/// not the payload, so mirror the HIR lowering and walk the named
/// children after `then_branch` for the first `block` / `if_stmt`.
fn find_else_branch<'a>(if_node: Node<'a>) -> Option<Node<'a>> {
    let then_id = if_node.child_by_field_name("then_branch")?.id();
    let mut cursor = if_node.walk();
    let mut seen_then = false;
    for c in if_node.named_children(&mut cursor) {
        if c.id() == then_id {
            seen_then = true;
            continue;
        }
        if !seen_then {
            continue;
        }
        if matches!(c.kind(), "block" | "if_stmt") {
            return Some(c);
        }
    }
    None
}

/// Strip the outer `{` / `}` from a block and re-indent its contents
/// to align with the column where the replaced node started. Leaves
/// blank lines untouched (any indentation on them is whitespace we
/// don't want to relocate). The first line keeps no leading indent
/// (the slot we're filling already sits at the outer column).
fn unwrap_block_to_outer_indent(
    text: &str,
    outer_anchor: usize,
    block_range: Range<usize>,
) -> String {
    let block_text = &text[block_range.clone()];
    let inner = block_text
        .strip_prefix('{')
        .unwrap_or(block_text)
        .strip_suffix('}')
        .unwrap_or(block_text);
    let inner = inner.trim_start_matches(['\n', '\r']);
    let inner = inner.trim_end_matches([' ', '\t', '\n', '\r']);

    // Outer indent: the whitespace prefix of the line on which
    // `outer_anchor` sits.
    let line_start = text[..outer_anchor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let outer_indent: String = text[line_start..outer_anchor]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();

    // Inner indent: leading whitespace of the first non-blank line.
    let inner_indent: String = inner
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect())
        .unwrap_or_default();

    let mut out = String::with_capacity(inner.len());
    for (i, line) in inner.lines().enumerate() {
        let stripped = line.strip_prefix(inner_indent.as_str()).unwrap_or(line);
        if i > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            // Don't re-indent blank lines.
            out.push_str(line);
        } else if i == 0 {
            // First line slots in at the outer anchor column — no
            // leading indent (the column is already occupied).
            out.push_str(stripped);
        } else {
            out.push_str(&outer_indent);
            out.push_str(stripped);
        }
    }
    out
}

/// Quickfix for `non-exhaustive`: append an `else if (rec == E::V) { … }`
/// arm for every missing variant, keyed off the diagnostic message and
/// the head `if_stmt`'s CST shape. The diagnostic's byte_range starts on
/// the head if; the splice point is `head.byte_range().end` (right after
/// the chain's trailing `}`), which keeps `} else if {` on the same line
/// in line with the existing chain style.
///
/// We *don't* offer a separate "add catch-all `else { }`" alternative
/// here — `edit_for_diagnostic` returns one fix per diagnostic, and the
/// missing-arm shape is the more useful default (each variant gets
/// explicit handling, the user can collapse to `else { }` themselves
/// in seconds). Extending the API to return multiple alternatives is a
/// larger surface change tracked separately.
fn non_exhaustive_fix(root: Node<'_>, text: &str, start: usize, message: &str) -> Vec<TextEdit> {
    let Some(head) = enclosing_kind(root, start, "if_stmt") else {
        return Vec::new();
    };
    let Some(receiver) = receiver_text_for_chain_head(text, head) else {
        return Vec::new();
    };
    let Some((enum_name, missing)) = parse_non_exhaustive_message(message) else {
        return Vec::new();
    };
    let head_range = head.byte_range();
    let indent = leading_whitespace_at(text, head_range.start);
    let body_indent_extra = "    ";

    let mut new_text = String::new();
    for variant in &missing {
        new_text.push_str(" else if (");
        new_text.push_str(receiver);
        new_text.push_str(" == ");
        new_text.push_str(enum_name);
        new_text.push_str("::");
        new_text.push_str(variant);
        new_text.push_str(") {\n");
        new_text.push_str(indent);
        new_text.push_str(body_indent_extra);
        new_text.push_str("// TODO: handle ");
        new_text.push_str(enum_name);
        new_text.push_str("::");
        new_text.push_str(variant);
        new_text.push('\n');
        new_text.push_str(indent);
        new_text.push('}');
    }

    vec![TextEdit {
        byte_range: head_range.end..head_range.end,
        new_text,
    }]
}

/// Walk up from `byte` until a node of `kind` is hit. Like
/// [`enclosing_node_range`] but returns the [`Node`] itself so the
/// caller can read fields / children.
fn enclosing_kind<'tree>(root: Node<'tree>, byte: usize, kind: &str) -> Option<Node<'tree>> {
    let mut node = root.descendant_for_byte_range(byte, byte)?;
    loop {
        if node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Read the receiver ident text from the head if's `binary_expr`
/// condition. The chain extractor only matches `ident == E::variant`
/// (or its mirror), so one operand is always an `ident` — return its
/// source text. The `static_expr` side carries the enum reference and
/// is handled separately via the message.
fn receiver_text_for_chain_head<'a>(text: &'a str, if_stmt: Node<'_>) -> Option<&'a str> {
    let cond = if_stmt.child_by_field_name("condition")?;
    if cond.kind() != "binary_expr" {
        return None;
    }
    let left = cond.child_by_field_name("left")?;
    let right = cond.child_by_field_name("right")?;
    let ident_node = match (left.kind(), right.kind()) {
        ("ident", _) => left,
        (_, "ident") => right,
        _ => return None,
    };
    Some(&text[ident_node.byte_range()])
}

/// Parse `non-exhaustive match over \`Foo\` (missing: a, b, c)` into
/// `("Foo", ["a","b","c"])`. Returns `None` on any deviation so the
/// quickfix bails out cleanly rather than emitting wrong code.
fn parse_non_exhaustive_message(message: &str) -> Option<(&str, Vec<&str>)> {
    let after_over = message.split_once("over `")?.1;
    let (enum_name, rest) = after_over.split_once('`')?;
    let after_missing = rest.split_once("missing:")?.1;
    let inside_parens = after_missing.split_once(')')?.0;
    let missing: Vec<&str> = inside_parens
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some((enum_name, missing))
}

/// Leading whitespace (spaces / tabs) on the line that contains `byte`,
/// up to `byte`. Used so injected arms inherit the existing chain's
/// column. Returns `""` when `byte` sits at the start of a line.
fn leading_whitespace_at(text: &str, byte: usize) -> &str {
    let line_start = text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = &text[line_start..byte];
    let ws_end = line
        .char_indices()
        .find(|(_, c)| !matches!(c, ' ' | '\t'))
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    &line[..ws_end]
}

// P23.3 follow-up
/// Fix for `unused-suppression`. The diagnostic's
/// `byte_range` points at the dead rule word inside a `// gcl-lint-…`
/// directive comment. Two shapes:
///
/// - **Multi-rule directive** (`// gcl-lint-next-off A B`, B is dead):
///   delete `B` plus its leading whitespace separator → leaves
///   `// gcl-lint-next-off A`. If `B` was the *first* rule, eat the
///   trailing whitespace instead so the result is `// gcl-lint-next-off
///   …rest…`.
/// - **Sole rule** (`// gcl-lint-next-off B`, B is dead): the directive
///   becomes useless when its only rule is removed; delete the entire
///   comment line (including any leading whitespace and the trailing
///   newline if the comment was the only content on the line).
///
/// Returns an empty Vec when the diagnostic byte range doesn't sit
/// inside a `line_comment`'s rule-list slot.
fn unused_suppression_fix(root: Node<'_>, text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let Some(comment_range) = enclosing_node_range(root, start, &["line_comment"]) else {
        return Vec::new();
    };
    let comment = &text[comment_range.clone()];
    let Some(rules) = comment_rule_word_ranges(comment) else {
        return Vec::new();
    };
    let rel_start = start - comment_range.start;
    let rel_end = end - comment_range.start;
    let Some(idx) = rules
        .iter()
        .position(|r| r.start == rel_start && r.end == rel_end)
    else {
        return Vec::new();
    };
    if rules.len() == 1 {
        return vec![TextEdit {
            byte_range: full_line_range_for_comment(text, &comment_range),
            new_text: String::new(),
        }];
    }
    // Multi-rule: drop this rule plus one whitespace separator.
    let bytes = text.as_bytes();
    let (del_start, del_end) = if idx == 0 {
        let mut j = end;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() && bytes[j] != b'\n' {
            j += 1;
        }
        (start, j)
    } else {
        let mut s = start;
        while s > comment_range.start && bytes[s - 1].is_ascii_whitespace() && bytes[s - 1] != b'\n'
        {
            s -= 1;
        }
        (s, end)
    };
    vec![TextEdit {
        byte_range: del_start..del_end,
        new_text: String::new(),
    }]
}

/// Fix for `empty-suppression` / `unbalanced-{lint,fmt}-off` — the
/// directive comment has no useful effect, so deleting it is the
/// minimal repair. Removes the whole comment line (and its trailing
/// newline if the comment was the sole content on the line) so the
/// rest of the file's blank-line vertical rhythm is preserved.
fn delete_comment_line_fix(root: Node<'_>, text: &str, byte: usize) -> Vec<TextEdit> {
    let Some(comment_range) = enclosing_node_range(root, byte, &["line_comment"]) else {
        return Vec::new();
    };
    vec![TextEdit {
        byte_range: full_line_range_for_comment(text, &comment_range),
        new_text: String::new(),
    }]
}

/// Walk a comment text (`// …`) and return the byte ranges of each
/// rule-list word — i.e. every whitespace-delimited word *after* the
/// directive name. Returns `None` for non-directive comments and for
/// directives that don't take rule lists (`gcl-fmt-…`).
fn comment_rule_word_ranges(comment: &str) -> Option<Vec<Range<usize>>> {
    let bytes = comment.as_bytes();
    if bytes.len() < 2 || &bytes[..2] != b"//" {
        return None;
    }
    let mut i = 2;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() && bytes[i] != b'\n' {
        i += 1;
    }
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let name = &comment[name_start..i];
    if !matches!(
        name,
        "gcl-lint-off" | "gcl-lint-on" | "gcl-lint-next-off" | "gcl-lint-file-off"
    ) {
        return None;
    }
    let mut rules: Vec<Range<usize>> = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let s = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if s < i {
            rules.push(s..i);
        }
    }
    Some(rules)
}

/// Compute the byte range to delete for a "remove the whole comment"
/// fix. When the comment is the only content on its line, we eat the
/// leading whitespace and the trailing newline so no blank line is
/// left behind. Otherwise, we delete just the comment span (preserving
/// surrounding code on the same line).
fn full_line_range_for_comment(text: &str, comment_range: &Range<usize>) -> Range<usize> {
    let line_start = text[..comment_range.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let leading_only_ws = text[line_start..comment_range.start]
        .chars()
        .all(|c| c.is_whitespace());
    if !leading_only_ws {
        return comment_range.clone();
    }
    let bytes = text.as_bytes();
    let mut end = comment_range.end;
    if end < bytes.len() && bytes[end] == b'\n' {
        end += 1;
    }
    line_start..end
}

fn enclosing_node_range(root: Node<'_>, byte: usize, kinds: &[&str]) -> Option<Range<usize>> {
    let mut node: Node<'_> = root.descendant_for_byte_range(byte, byte)?;
    loop {
        if kinds.contains(&node.kind()) {
            return Some(node.byte_range());
        }
        node = node.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;

    /// CST-only fix invocation. Used by rule tests that don't depend
    /// on scope information (everything other than the scope-aware
    /// `unused-local` rename path).
    fn fix_with_msg(code: &str, text: &str, range: Range<usize>, message: &str) -> Vec<TextEdit> {
        let tree = parse(text);
        let cx = QuickfixCx::from_cst(tree.root_node(), text);
        edit_for_diagnostic(&cx, code, &range, message)
    }

    fn fix(code: &str, text: &str, range: Range<usize>) -> Vec<TextEdit> {
        fix_with_msg(code, text, range, "")
    }

    /// HIR-backed fix invocation. Use for rules whose fix consults
    /// scope information (currently `unused-local`'s rename-on-impure-
    /// init path, for collision-free `_name` synthesis).
    fn fix_with_scope(code: &str, text: &str, range: Range<usize>) -> Vec<TextEdit> {
        let tree = parse(text);
        let symbols = SymbolTable::new();
        let hir = lower_module(text, &symbols, "mod", "project", tree.root_node());
        let cx = QuickfixCx {
            root: tree.root_node(),
            text,
            hir: Some(&hir),
            symbols: Some(&symbols),
        };
        edit_for_diagnostic(&cx, code, &range, "")
    }

    #[test]
    fn infer_return_type_inserts_annotation_after_params() {
        // The lint's byte range covers the fn name. The fix should
        // synthesize an annotation `: float?` and place it between
        // the closing `)` of the params and the opening `{` of the
        // body, leaving everything else untouched.
        let src = "type Foo { b: float?; }\nfn foo(x: Foo) {\n    return x.b;\n}\n";
        let name_start = src.find("fn foo").unwrap() + 3;
        let edits = fix_with_msg(
            "infer-return-type",
            src,
            name_start..(name_start + 3),
            "return type can be inferred as `float?`",
        );
        assert_eq!(edits.len(), 1);
        let params_end = src.find(") {").unwrap() + 1;
        assert_eq!(edits[0].byte_range, params_end..params_end);
        assert_eq!(edits[0].new_text, ": float?");
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("fn foo(x: Foo): float? {"),
            "edit didn't produce the expected shape:\n{after}"
        );
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    #[test]
    fn infer_return_type_works_on_type_method() {
        // Same shape on a method inside a type body. The fix must walk
        // up to the `type_method` node, not stop at a `fn_decl` it
        // never finds.
        let src =
            "type Box {\n    count: int;\n    fn get() {\n        return this.count;\n    }\n}\n";
        let name_start = src.find("fn get").unwrap() + 3;
        let edits = fix_with_msg(
            "infer-return-type",
            src,
            name_start..(name_start + 3),
            "return type can be inferred as `int`",
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, ": int");
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("fn get(): int {"),
            "edit didn't produce the expected shape:\n{after}"
        );
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    #[test]
    fn infer_return_type_skips_when_fn_already_has_return_type() {
        // Defensive: a stale diagnostic from a buffer that the user
        // already annotated must not double-annotate. The fix queries
        // the CST for an existing `return_type` field and bails.
        let src = "fn foo(x: int): int {\n    return x;\n}\n";
        let name_start = src.find("fn foo").unwrap() + 3;
        let edits = fix_with_msg(
            "infer-return-type",
            src,
            name_start..(name_start + 3),
            "return type can be inferred as `int`",
        );
        assert!(
            edits.is_empty(),
            "fix on an already-annotated fn must no-op, got: {edits:?}"
        );
    }

    #[test]
    fn infer_return_type_skips_when_message_carries_no_type() {
        // If the lint message somehow loses its backticked type
        // (synthetic test, shouldn't happen at runtime), the fix bails
        // rather than synthesize a malformed `: ` annotation.
        let src = "fn foo() {\n    return 42;\n}\n";
        let name_start = src.find("fn foo").unwrap() + 3;
        let edits = fix_with_msg(
            "infer-return-type",
            src,
            name_start..(name_start + 3),
            "return type can be inferred (no backticks here)",
        );
        assert!(edits.is_empty(), "fix without backticks should bail");
    }

    #[test]
    fn unused_local_renames_for_in_binder() {
        let src = "fn f(vars: Array<String>) {\n    for (k, v in vars) {}\n}\n";
        let k_start = src.find("(k").unwrap() + 1;
        let edits = fix("unused-local", src, k_start..(k_start + 1));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, k_start..(k_start + 1));
        assert_eq!(edits[0].new_text, "_k");
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    #[test]
    fn unused_local_renames_for_init_var() {
        let src = "fn f() {\n    for (var i = 0; false; 0) {}\n}\n";
        let i_start = src.find("var i").unwrap() + 4;
        let edits = fix("unused-local", src, i_start..(i_start + 1));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, i_start..(i_start + 1));
        assert_eq!(edits[0].new_text, "_i");
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    #[test]
    fn unused_local_removes_whole_var_stmt_when_init_is_pure() {
        // `var foo = 42;` — pure literal initializer, no side effect to
        // preserve. Fix should expand to the full statement and drop it.
        let src = "fn f() {\n    var foo = 42;\n    return 0;\n}\n";
        let foo_start = src.find("foo").unwrap();
        let edits = fix("unused-local", src, foo_start..(foo_start + 3));
        assert_eq!(edits.len(), 1);
        let stmt_start = src.find("var foo").unwrap();
        let stmt_end = src.find(";\n    return").unwrap() + 1;
        assert_eq!(edits[0].byte_range, stmt_start..stmt_end);
        assert_eq!(edits[0].new_text, "");
        // Apply & re-parse: must be syntactically valid.
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    // Reproducer for the side-effect-eating bug: `var unused = call();`
    // must not have its whole statement deleted (the call would be
    // silently dropped). The fix renames the binder to `_unused` so the
    // initializer (and its side effects) still execute.
    #[test]
    fn unused_local_keeps_var_when_init_has_side_effects() {
        let src = "\
fn main() {
    var unused = anything_with_sideeffect();
}

fn anything_with_sideeffect() {}
";
        let name_start = src.find("unused").unwrap();
        let edits = fix_with_scope("unused-local", src, name_start..(name_start + 6));
        assert_eq!(edits.len(), 1, "expected exactly one edit, got {edits:?}");
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("anything_with_sideeffect()"),
            "fix nuked the side-effecting call site:\nbefore:\n{src}\nafter:\n{after}"
        );
        assert!(
            after.contains("var _unused = anything_with_sideeffect();"),
            "expected `var _unused = anything_with_sideeffect();`, got:\n{after}"
        );
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    // The rename target `_unused` already exists in the local scope, so
    // the fix must escalate to `_unused_2` to avoid shadowing.
    #[test]
    fn unused_local_renames_with_suffix_on_collision() {
        let src = "\
fn main() {
    var _unused = 0;
    var unused = side_effect();
}

fn side_effect() {}
";
        let name_start = src.find("var unused").unwrap() + 4;
        let edits = fix_with_scope("unused-local", src, name_start..(name_start + 6));
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("var _unused_2 = side_effect();"),
            "expected `_unused_2`, got:\n{after}"
        );
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    // Both `_unused` and `_unused_2` are taken; the fix picks
    // `_unused_3`.
    #[test]
    fn unused_local_renames_with_higher_suffix_when_chain_collides() {
        let src = "\
fn main() {
    var _unused = 0;
    var _unused_2 = 1;
    var unused = side_effect();
}

fn side_effect() {}
";
        let name_start = src.find("var unused").unwrap() + 4;
        let edits = fix_with_scope("unused-local", src, name_start..(name_start + 6));
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("var _unused_3 = side_effect();"),
            "expected `_unused_3`, got:\n{after}"
        );
    }

    // Pure initializer, but the fn body has side effects elsewhere —
    // the var-stmt itself is still safe to delete.
    #[test]
    fn unused_local_with_pure_ident_init_is_deleted() {
        let src = "fn f(x: int) {\n    var foo = x;\n    return 0;\n}\n";
        let foo_start = src.find("foo").unwrap();
        let edits = fix("unused-local", src, foo_start..(foo_start + 3));
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            !after.contains("var foo"),
            "expected stmt deleted:\n{after}"
        );
    }

    // No initializer at all — safe to delete unconditionally.
    #[test]
    fn unused_local_without_initializer_is_deleted() {
        let src = "fn f() {\n    var foo: int;\n    return 0;\n}\n";
        let foo_start = src.find("foo").unwrap();
        let edits = fix("unused-local", src, foo_start..(foo_start + 3));
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            !after.contains("var foo"),
            "expected stmt deleted:\n{after}"
        );
    }

    #[test]
    fn unused_decl_removes_whole_fn() {
        let src = "private fn helper() {}\n\nfn main() {}\n";
        let helper_start = src.find("helper").unwrap();
        let edits = fix("unused-decl", src, helper_start..(helper_start + 6));
        assert_eq!(edits.len(), 1);
        let decl_start = 0; // beginning of "private fn ..."
        let decl_end = src.find("\n\nfn main").unwrap();
        assert_eq!(edits[0].byte_range, decl_start..decl_end);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "applied edit produced parse error:\n{after}"
        );
    }

    #[test]
    fn unused_param_renames_when_body_doesnt_use_it() {
        let src = "fn f(unused: int) { var x = 0; }\n";
        let p_start = src.find("unused").unwrap();
        let edits = fix("unused-param", src, p_start..(p_start + 6));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "_unused");
    }

    fn apply(src: &str, edits: &[TextEdit]) -> String {
        let mut out = src.to_string();
        for e in edits.iter().rev() {
            out.replace_range(e.byte_range.clone(), &e.new_text);
        }
        out
    }

    #[test]
    fn unused_generic_param_strips_only_generic_block() {
        let src = "type Foo<T> { a: int; }\n";
        let t = src.find('T').unwrap();
        let edits = fix("unused-generic-param", src, t..(t + 1));
        assert_eq!(edits.len(), 1);
        let after = apply(src, &edits);
        assert_eq!(after, "type Foo { a: int; }\n");
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "produced parse error: {after}"
        );
    }

    #[test]
    fn unused_generic_param_strips_leading_param() {
        let src = "type Map<K, V> { a: V; }\n";
        let k = src.find('K').unwrap();
        let edits = fix("unused-generic-param", src, k..(k + 1));
        assert_eq!(edits.len(), 1);
        let after = apply(src, &edits);
        assert_eq!(after, "type Map<V> { a: V; }\n");
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "produced parse error: {after}"
        );
    }

    #[test]
    fn unused_generic_param_strips_trailing_param() {
        let src = "type Map<K, V> { a: K; }\n";
        let v = src.find(", V").unwrap() + 2;
        let edits = fix("unused-generic-param", src, v..(v + 1));
        assert_eq!(edits.len(), 1);
        let after = apply(src, &edits);
        assert_eq!(after, "type Map<K> { a: K; }\n");
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "produced parse error: {after}"
        );
    }

    #[test]
    fn unused_generic_param_strips_middle_param_in_three_arity() {
        let src = "fn foo<A, B, C>(a: A, c: C): A { return a; }\n";
        let b = src.find(", B").unwrap() + 2;
        let edits = fix("unused-generic-param", src, b..(b + 1));
        assert_eq!(edits.len(), 1);
        let after = apply(src, &edits);
        assert_eq!(after, "fn foo<A, C>(a: A, c: C): A { return a; }\n");
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "produced parse error: {after}"
        );
    }

    #[test]
    fn unused_generic_param_strips_fn_generic() {
        let src = "fn foo<T>(x: int): int { return x; }\n";
        let t = src.find('T').unwrap();
        let edits = fix("unused-generic-param", src, t..(t + 1));
        assert_eq!(edits.len(), 1);
        let after = apply(src, &edits);
        assert_eq!(after, "fn foo(x: int): int { return x; }\n");
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "produced parse error: {after}"
        );
    }

    #[test]
    fn unused_generic_param_skipped_when_underscore_prefixed() {
        let src = "type Foo<_T> { a: int; }\n";
        let t = src.find("_T").unwrap();
        let edits = fix("unused-generic-param", src, t..(t + 2));
        assert!(edits.is_empty(), "expected refusal, got {edits:?}");
    }

    #[test]
    fn missing_token_inserts_quoted_token() {
        let edits = fix_with_msg("missing-token", "ab", 2..2, "missing `;`");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, 2..2);
        assert_eq!(edits[0].new_text, ";");
    }

    #[test]
    fn redundant_non_null_assertion_drops_slice() {
        // Range = the `!!` slice. Fix replaces with empty.
        let src = "fn f() { var x = bar()!!; }\n";
        let bb_start = src.find("!!").unwrap();
        let edits = fix(
            "redundant-non-null-assertion",
            src,
            bb_start..(bb_start + 2),
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, bb_start..(bb_start + 2));
        assert_eq!(edits[0].new_text, "");
    }

    // -----------------------------------------------------------------
    // P23 — directive-comment quickfixes
    // -----------------------------------------------------------------

    #[test]
    fn unused_suppression_drops_dead_rule_from_multi_rule_directive() {
        // `unused-param` is dead → fix should remove just that word
        // plus its leading space, leaving `// gcl-lint-next-off unused-local`.
        let src =
            "fn main() {\n    // gcl-lint-next-off unused-local unused-param\n    var x = 42;\n}\n";
        let dead = src.find("unused-param").unwrap();
        let edits = fix(
            "unused-suppression",
            src,
            dead..(dead + "unused-param".len()),
        );
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("// gcl-lint-next-off unused-local\n"),
            "after = {after:?}"
        );
        assert!(!after.contains("unused-param"), "after = {after:?}");
    }

    #[test]
    fn unused_suppression_drops_first_rule_eats_trailing_space() {
        let src =
            "fn main() {\n    // gcl-lint-next-off unused-local unused-param\n    var y = 0;\n}\n";
        let dead = src.find("unused-local").unwrap();
        let edits = fix(
            "unused-suppression",
            src,
            dead..(dead + "unused-local".len()),
        );
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            after.contains("// gcl-lint-next-off unused-param\n"),
            "after = {after:?}"
        );
    }

    #[test]
    fn unused_suppression_on_sole_rule_deletes_whole_comment_line() {
        let src = "fn main() {\n    // gcl-lint-next-off unused-param\n    var y = 0;\n}\n";
        let dead = src.find("unused-param").unwrap();
        let edits = fix(
            "unused-suppression",
            src,
            dead..(dead + "unused-param".len()),
        );
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            !after.contains("gcl-lint-next-off"),
            "expected the whole directive line gone, after = {after:?}"
        );
        // The leading 4-space indent should also be eaten so no blank
        // line is left behind.
        assert!(
            after.contains("fn main() {\n    var y"),
            "expected no leftover blank line, after = {after:?}"
        );
    }

    // -----------------------------------------------------------------
    // P24.6 — `unreachable` quickfix
    // -----------------------------------------------------------------

    #[test]
    fn unreachable_fix_deletes_post_return_dead_stmt() {
        let src = "fn f(): int { return 1; var _ = 0; }";
        let dead_start = src.find("var _ = 0;").unwrap();
        let dead_end = dead_start + "var _ = 0;".len();
        let edits = fix("unreachable", src, dead_start..dead_end);
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(
            !after.contains("var _ = 0;"),
            "expected dead stmt removed, after = {after:?}"
        );
        // Re-parse must succeed.
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "fix would have introduced parse errors: {after}"
        );
    }

    #[test]
    fn unreachable_fix_swallows_else_keyword_for_dead_else_block() {
        // The dead-else case: the diagnostic's range covers `{ … }`.
        // The fix must also delete the leading `else` keyword + the
        // whitespace between the prior `}` and the `else`.
        let src = "fn f(): int {\n    if (true) {\n        return 1;\n    } else {\n        return 2;\n    }\n}\n";
        // The dead else block is the SECOND `{...}` chunk. Compute the
        // end as `(start of else { + offset to the matching `}`)`.
        let dead_block_start = src.find("else {").unwrap() + "else ".len();
        // The dead block ends at the `}` that closes the `else { … }`
        // body — that's the SECOND `}` from the start of `else`.
        let after_open = dead_block_start + 1;
        let dead_block_end = src[after_open..]
            .find('}')
            .map(|i| after_open + i + 1)
            .unwrap();
        let edits = fix("unreachable", src, dead_block_start..dead_block_end);
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        // The `else` keyword should be gone alongside the block.
        assert!(
            !after.contains("else"),
            "expected `else` keyword swallowed, after = {after:?}"
        );
        let tree = greycat_analyzer_syntax::parse(&after);
        assert!(
            !tree.root_node().has_error(),
            "fix would have introduced parse errors: {after}"
        );
    }

    #[test]
    fn empty_suppression_deletes_whole_comment_line() {
        let src = "fn main() {\n    // gcl-lint-off\n    var x = 1;\n}\n";
        let comment_start = src.find("// gcl-lint-off").unwrap();
        // empty-suppression's diagnostic byte_range covers the whole
        // comment (matches what the directive parser emits).
        let comment_end = src[comment_start..].find('\n').unwrap() + comment_start;
        let edits = fix("empty-suppression", src, comment_start..comment_end);
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        assert!(!after.contains("gcl-lint-off"), "after = {after:?}");
    }

    // -----------------------------------------------------------------
    // `non-exhaustive` quickfix
    // -----------------------------------------------------------------

    #[test]
    fn non_exhaustive_appends_else_if_arms_for_each_missing_variant() {
        let src = "\
enum E { foo, bar, baz, qux }
fn t(e: E) {
    if (e == E::foo) {
    } else if (e == E::bar) {
    }
}
";
        let head = src.find("if (e == E::foo)").unwrap();
        // Use a realistic message — the quickfix parses missing variants
        // out of it. The quickfix dispatcher requires the right `code`,
        // and the message is what the lint emitter actually produces.
        let msg = "non-exhaustive match over `E` (missing: baz, qux)";
        let edits = fix_with_msg(
            "non-exhaustive",
            src,
            head..head + "if (e == E::foo)".len(),
            msg,
        );
        assert_eq!(edits.len(), 1);
        let mut after = src.to_string();
        after.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
        // Both missing arms appear, in order, with same-line `} else if`.
        assert!(
            after.contains("} else if (e == E::baz) {"),
            "expected `else if E::baz` arm, after =\n{after}"
        );
        assert!(
            after.contains("} else if (e == E::qux) {"),
            "expected `else if E::qux` arm, after =\n{after}"
        );
        // Indentation matches the head `if`'s 4-space indent (chain
        // sits inside `fn t(e: E) {`, so head is at 4 spaces, bodies at 8).
        assert!(
            after.contains("    } else if (e == E::baz) {\n        // TODO: handle E::baz\n    }"),
            "expected indented arm body, after =\n{after}"
        );
    }

    #[test]
    fn non_exhaustive_no_edits_when_message_doesnt_parse() {
        // Defensive: a malformed message must not produce edits.
        let src = "fn t(e: E) { if (e == E::foo) {} else if (e == E::bar) {} }\n";
        let head = src.find("if (e == E::foo)").unwrap();
        let edits = fix_with_msg(
            "non-exhaustive",
            src,
            head..head + 4,
            "totally not the expected shape",
        );
        assert!(edits.is_empty());
    }

    #[test]
    fn redundant_semicolon_deletes_the_range() {
        // The diagnostic byte range covers exactly the
        // `block_trailing_semi` CST node, so the quickfix is a plain
        // slice-delete.
        let src = "fn n(): int { return 1; };\n";
        let semi = src.find(";\n").unwrap();
        let edits = fix("redundant-semicolon", src, semi..semi + 1);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, semi..semi + 1);
        assert_eq!(edits[0].new_text, "");
    }

    #[test]
    fn redundant_semicolon_deletes_multi_semi_run() {
        // `};;;` -> `}` (entire run is one diagnostic range).
        let src = "fn n() {};;;\n";
        let run_start = src.find(";;;").unwrap();
        let edits = fix("redundant-semicolon", src, run_start..run_start + 3);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].byte_range, run_start..run_start + 3);
        assert_eq!(edits[0].new_text, "");
    }
}
