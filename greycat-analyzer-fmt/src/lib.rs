//! Formatter for `.gcl` source.
//!
//! Two layers stacked on a small Wadler/Leijen pretty printer:
//!
//! - [`doc`] — the layout IR (`Doc::Text`, `Doc::Group`, `Doc::Indent`,
//!   `Doc::Line`, `Doc::Hard`, `Doc::IfBroken`, `Doc::BlankLine`).
//! - [`render`] — width-aware printer that picks flat-vs-broken per
//!   `Group` via fits-flat measurement.
//!
//! P21 is rebuilding the CST → Doc lowering on top of these primitives;
//! while that work lands, [`format`] / [`format_tree`] still route
//! through the legacy single-pass walker (in this file). New chunks
//! flip individual constructs over to the new pipeline as their
//! lowering visitors land.

pub mod doc;
pub mod render;
pub mod trivia;

use greycat_analyzer_syntax::tree_sitter::{Node, TreeCursor};

const INDENT: &str = "    ";

/// Layout options. Defaults match the TS reference (`cst_format.ts`).
#[derive(Debug, Clone, Copy)]
pub struct FmtOptions {
    /// Maximum line width before a `Group` breaks. TS default: 120.
    pub line_width: usize,
    /// Spaces per indent step. TS default: 4.
    pub indent: usize,
    /// Append a trailing newline at end of file. TS default: false.
    /// We default to `true` because real-world files carry one and the
    /// playground / LSP path preserves it; the parity gauntlet's fixtures
    /// don't (the corpus `out.gcl` files are saved without one).
    pub eol_last: bool,
}

impl Default for FmtOptions {
    fn default() -> Self {
        FmtOptions {
            line_width: 120,
            indent: 4,
            eol_last: false,
        }
    }
}

/// Format `source` and return the canonical text. A best-effort pass —
/// see the module docs for parity caveats.
pub fn format(source: &str) -> String {
    let tree = greycat_analyzer_syntax::parse(source);
    format_tree(source, tree.root_node())
}

/// Format a pre-parsed CST. Useful when callers already have a tree
/// from incremental parsing (LSP, editor integrations).
pub fn format_tree(source: &str, root: Node<'_>) -> String {
    let mut out = String::new();
    let mut cursor = root.walk();
    let mut state = State {
        indent: 0,
        last_byte: 0,
        last_emitted: None,
        suppress_space: false,
        last_was_doc: false,
    };
    walk(source, &mut cursor, &mut out, &mut state);
    // Trail a single newline iff the input did. Mirrors the TS
    // prettifier's behavior — the corpus's `out.gcl` fixtures don't
    // carry a trailing newline, but real-world files usually do, and
    // we shouldn't strip the user's existing one.
    if source.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    } else if !source.ends_with('\n') {
        while out.ends_with('\n') {
            out.pop();
        }
    }
    out
}

#[derive(Default)]
struct State {
    indent: usize,
    /// Byte offset just past the last *named* node we wrote — used to
    /// preserve the user's blank-line breaks between top-level items.
    last_byte: usize,
    last_emitted: Option<EmittedKind>,
    suppress_space: bool,
    /// `true` after emitting a `doc_comment`. The next token suppresses
    /// blank-line preservation so doc-comments stick to their decl.
    last_was_doc: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmittedKind {
    OpenBrace,
    CloseBrace,
    Semicolon,
    Comma,
    Dot,
    Arrow,
    LParen,
    RParen,
    Other,
    Newline,
}

fn walk(source: &str, cursor: &mut TreeCursor<'_>, out: &mut String, state: &mut State) {
    loop {
        let node = cursor.node();
        if cursor.goto_first_child() {
            walk(source, cursor, out, state);
            cursor.goto_parent();
            after_container(source, node, out, state);
        } else {
            emit_token(source, node, out, state);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Hook fired after we finish walking the children of `node`. Lets us
/// emit per-construct trailing whitespace (e.g. force a newline after
/// an `annotations` group at the top of a decl).
fn after_container(_source: &str, node: Node<'_>, out: &mut String, state: &mut State) {
    if node.kind() == "annotations" && !out.ends_with('\n') {
        push_newline(out, state);
    }
}

fn emit_token(source: &str, node: Node<'_>, out: &mut String, state: &mut State) {
    let text = source.get(node.byte_range()).unwrap_or("");
    if text.is_empty() {
        return;
    }
    let kind = node.kind();

    // Preserve user-intent blank lines between top-level / sibling
    // decls. Count `\n`s in the source between the previous emitted
    // node's end and this node's start; when the user had >= 2
    // newlines, emit blank lines to match (capped at 2 to keep the
    // formatter's output sane).
    if state.last_byte > 0 && node.start_byte() >= state.last_byte && !state.last_was_doc {
        let between = source.get(state.last_byte..node.start_byte()).unwrap_or("");
        let nl_count = between.matches('\n').count();
        if nl_count >= 2 {
            let extras = (nl_count - 1).min(3);
            // Make sure we're at line start before emitting blanks.
            if !out.ends_with('\n') {
                push_newline(out, state);
            }
            for _ in 0..extras {
                out.push('\n');
            }
            state.last_emitted = Some(EmittedKind::Newline);
            state.suppress_space = true;
        }
    }

    match kind {
        "{" => {
            ensure_leading_space(out, state);
            out.push('{');
            // Empty container: `block {}` / `type_body {}` etc. Don't
            // push newlines — let the matching `}` handler emit on the
            // same line.
            let is_empty_container = node
                .parent()
                .map(|p| {
                    matches!(
                        p.kind(),
                        "block" | "type_body" | "object_initializers" | "object_fields"
                    ) && p.named_child_count() == 0
                })
                .unwrap_or(false);
            if !is_empty_container {
                state.indent += 1;
                push_newline(out, state);
            }
            state.last_emitted = Some(EmittedKind::OpenBrace);
        }
        "}" => {
            // type_body's grammar made trailing `;` optional in P7.1, but
            // the formatter still wants every type_attr to end with `;`
            // for parity with the TS prettifier. If the closing `}` of a
            // `type_body` lands on a line whose content doesn't end with
            // `;`, append one before re-indenting.
            if let Some(parent) = node.parent()
                && parent.kind() == "type_body"
            {
                trim_trailing_spaces(out);
                if !out.ends_with('\n')
                    && !out.ends_with(';')
                    && !out.ends_with('{')
                    && !out.is_empty()
                {
                    out.push(';');
                }
            }
            // Empty container: `}` follows `{` on the same line.
            let is_empty_container = node
                .parent()
                .map(|p| {
                    matches!(
                        p.kind(),
                        "block" | "type_body" | "object_initializers" | "object_fields"
                    ) && p.named_child_count() == 0
                })
                .unwrap_or(false);
            if is_empty_container {
                out.push('}');
            } else {
                trim_trailing_spaces(out);
                if !out.ends_with('\n') {
                    push_newline(out, state);
                }
                state.indent = state.indent.saturating_sub(1);
                push_indent(out, state);
                out.push('}');
            }
            state.last_emitted = Some(EmittedKind::CloseBrace);
        }
        ";" => {
            trim_trailing_spaces(out);
            out.push(';');
            push_newline(out, state);
            state.last_emitted = Some(EmittedKind::Semicolon);
        }
        "," => {
            trim_trailing_spaces(out);
            out.push(',');
            out.push(' ');
            state.last_emitted = Some(EmittedKind::Comma);
        }
        "." => {
            trim_trailing_spaces(out);
            out.push('.');
            state.last_emitted = Some(EmittedKind::Dot);
            state.suppress_space = true;
        }
        "->" => {
            trim_trailing_spaces(out);
            out.push_str("->");
            state.last_emitted = Some(EmittedKind::Arrow);
            state.suppress_space = true;
        }
        // `:` between an ident and its type — no space before, space
        // after. Covers fn-param `name: type`, `var name: type`,
        // type-attr `name: type`, and the type_decorator `: type` slot.
        // Excludes `::` (a separate token kind).
        ":" => {
            trim_trailing_spaces(out);
            out.push(':');
            out.push(' ');
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = true;
        }
        "::" => {
            trim_trailing_spaces(out);
            out.push_str("::");
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = true;
        }
        // `?` for optional types / null-coalesce — no space before, default
        // (space) after.
        "?" => {
            trim_trailing_spaces(out);
            out.push('?');
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = false;
        }
        // `@` opens an annotation — emit verbatim, no surrounding space, so
        // `@library`, `@expose`, etc. format tightly.
        "@" => {
            push_indent_if_at_line_start(out, state);
            out.push('@');
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = true;
        }
        // `<` / `>` are dual-use. Generics (`Array<T>`) want tight
        // spacing; comparisons (`x < z`) want spaces around. Discriminate
        // by parent kind: generic containers in the grammar are
        // `type_params` and `type_ident` (which carries `< … >` for
        // nested instantiations).
        "<" | ">" => {
            let in_generic = node
                .parent()
                .map(|p| matches!(p.kind(), "type_params" | "type_ident"))
                .unwrap_or(false);
            if in_generic {
                trim_trailing_spaces(out);
                out.push_str(text);
                state.last_emitted = Some(EmittedKind::Other);
                state.suppress_space = true;
            } else {
                emit_binary_op(out, state, text);
            }
        }
        // Binary / assignment operators — surround with spaces.
        "=" | "==" | "!=" | "<=" | ">=" | "+" | "-" | "*" | "/" | "%" | "&&" | "||" | "+="
        | "-=" | "*=" | "/=" | "%=" | "??" => {
            emit_binary_op(out, state, text);
        }
        // Unary `!` — no space after, possibly space before (handled
        // by ensure_leading_space).
        "!" => {
            ensure_leading_space(out, state);
            out.push('!');
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = true;
        }
        "(" | "[" => {
            push_indent_if_at_line_start(out, state);
            // No leading space before `(` after an ident / type ident
            // (call site, fn signature, generic args). Allow space after
            // a control-flow keyword like `if` / `while` / `for`.
            let prev_text = last_word(out);
            let needs_space = matches!(
                prev_text.as_deref(),
                Some("if")
                    | Some("while")
                    | Some("for")
                    | Some("do")
                    | Some("return")
                    | Some("throw")
            );
            if needs_space && !out.ends_with(' ') && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::LParen);
            state.suppress_space = true;
        }
        ")" | "]" => {
            trim_trailing_spaces(out);
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::RParen);
            state.suppress_space = false;
        }
        "doc_comment" | "line_comment" => {
            // EOL comments — when the source has no newline between
            // the previous token and this comment, keep them on the
            // same line. Otherwise, the comment is on its own line and
            // should respect the current indent.
            //
            // Note: the previous token's emitter (e.g. `{`, `;`) may
            // have already pushed a newline. We use the *source*
            // between bytes to decide intent, then pop the trailing
            // whitespace if needed to re-attach the comment inline.
            let between = source.get(state.last_byte..node.start_byte()).unwrap_or("");
            let inline_in_source = !between.contains('\n') && state.last_byte > 0;
            if kind == "line_comment" && inline_in_source {
                // Drop any trailing whitespace + a single newline
                // that previous token rules pushed eagerly.
                while let Some(c) = out.chars().next_back() {
                    if c == '\n' || c == ' ' || c == '\t' {
                        out.pop();
                    } else {
                        break;
                    }
                }
                out.push(' ');
                out.push_str(text);
                push_newline(out, state);
            } else {
                push_indent_if_at_line_start(out, state);
                out.push_str(text);
                push_newline(out, state);
            }
            state.last_emitted = Some(EmittedKind::Newline);
        }
        _ => {
            ensure_leading_space(out, state);
            push_indent_if_at_line_start(out, state);
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = false;
        }
    }
    state.last_byte = node.end_byte();
    state.last_was_doc = matches!(kind, "doc_comment" | "line_comment");
}

/// Emit a binary operator — `<op>` surrounded by spaces. Wraps the
/// pre-emit cleanup so the previous token's trailing whitespace is
/// canonicalized.
fn emit_binary_op(out: &mut String, state: &mut State, text: &str) {
    trim_trailing_spaces(out);
    if !out.ends_with('\n') && !out.is_empty() {
        out.push(' ');
    }
    out.push_str(text);
    out.push(' ');
    state.last_emitted = Some(EmittedKind::Other);
    state.suppress_space = true;
}

/// Push a single ASCII space when one is *needed* — i.e. the previous
/// emitted text is "word-shaped" (last char is ident-y), and the
/// suppress_space flag isn't asking us to skip. Skips when at the
/// start of a fresh line (since indent will fire next).
fn ensure_leading_space(out: &mut String, state: &State) {
    if state.suppress_space {
        return;
    }
    if out.is_empty() || out.ends_with('\n') || out.ends_with(' ') {
        return;
    }
    let last = out.chars().next_back();
    let needs = matches!(
        last,
        Some(c)
            if c.is_ascii_alphanumeric()
                || c == '_'
                || c == ')'
                || c == ']'
                || c == '}'
                || c == '?'
                || c == '"'
    );
    if needs {
        out.push(' ');
    }
}

/// Return the last word emitted into `out` — the trailing run of
/// `[A-Za-z0-9_]+`. Used to disambiguate `if (` (keyword + space + `(`)
/// from `foo(` (call site, no space).
fn last_word(out: &str) -> Option<String> {
    let bytes = out.as_bytes();
    let end = bytes.len();
    let mut start = end;
    while start > 0 {
        let b = bytes[start - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    Some(out[start..].to_string())
}

fn push_newline(out: &mut String, state: &mut State) {
    out.push('\n');
    state.last_emitted = Some(EmittedKind::Newline);
    state.suppress_space = true;
}

fn push_indent(out: &mut String, state: &State) {
    for _ in 0..state.indent {
        out.push_str(INDENT);
    }
}

fn push_indent_if_at_line_start(out: &mut String, state: &mut State) {
    if out.ends_with('\n') {
        push_indent(out, state);
        state.suppress_space = true;
        state.last_emitted = Some(EmittedKind::Other);
    }
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(src: &str) -> String {
        let formatted = format(src);
        // Verify the output re-parses cleanly.
        let tree = greycat_analyzer_syntax::parse(&formatted);
        assert!(
            !tree.root_node().has_error(),
            "formatted output failed to re-parse:\n--- input ---\n{src}\n--- output ---\n{formatted}\n--- sexp ---\n{}",
            tree.root_node().to_sexp(),
        );
        formatted
    }

    #[test]
    fn empty_module_is_empty() {
        // P14.3 changed format() to mirror the input's trailing-newline
        // policy. Empty input → empty output (no synthetic newline).
        let out = format("");
        assert_eq!(out, "");
    }

    #[test]
    fn single_fn_round_trips() {
        let src = "fn main() {}\n";
        let out = roundtrip(src);
        // Body should be on its own line, indented or empty.
        assert!(out.contains("fn main()"), "got: {out:?}");
    }

    #[test]
    fn type_decl_round_trips() {
        let src = r#"
type Foo {
    a: int;
    b: float;
}
"#;
        let _ = roundtrip(src);
    }

    #[test]
    fn fmt_re_fmt_is_idempotent_on_simple_input() {
        let src = "fn add(a: int, b: int): int { return a + b; }\n";
        let once = format(src);
        let twice = format(&once);
        assert_eq!(
            once, twice,
            "expected idempotency, got\n{once}\n!=\n{twice}"
        );
    }

    #[test]
    fn enum_round_trips() {
        let src = "enum Color { Red, Green, Blue }\n";
        let _ = roundtrip(src);
    }
}
