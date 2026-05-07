//! Formatter for `.gcl` source (P4.1).
//!
//! Walks the tree-sitter CST and emits a normalized source string with
//! consistent indentation and spacing. This is a *foundational* port —
//! enough to round-trip representative fixtures through `parse → fmt →
//! parse` without errors. **Byte-for-byte parity with the TS prettifier**
//! (M5 acceptance criterion) is explicitly out of scope for this pass;
//! the TS port lives at `packages/lang/src/parser/cst/cst_format.ts`
//! (~1,354 LoC of cases) and a faithful port is its own milestone.
//!
//! The algorithm is a single linear walk in source order using a
//! `TreeCursor`. Each named/anonymous token contributes its source text
//! plus a small per-kind rule for whitespace before/after.

use greycat_analyzer_syntax::tree_sitter::{Node, TreeCursor};

const INDENT: &str = "    ";

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
    };
    walk(source, &mut cursor, &mut out, &mut state);
    // Always trail a single newline — most editors / formatters expect it.
    if !out.ends_with('\n') {
        out.push('\n');
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
            // Pre-visit: if this is a structural opener, emit it.
            walk(source, cursor, out, state);
            cursor.goto_parent();
        } else {
            emit_token(source, node, out, state);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn emit_token(source: &str, node: Node<'_>, out: &mut String, state: &mut State) {
    let text = source.get(node.byte_range()).unwrap_or("");
    if text.is_empty() {
        return;
    }
    let kind = node.kind();
    match kind {
        "{" => {
            if needs_leading_space(state) {
                out.push(' ');
            }
            out.push('{');
            state.indent += 1;
            push_newline(out, state);
            state.last_emitted = Some(EmittedKind::OpenBrace);
        }
        "}" => {
            // strip trailing whitespace/spaces before `}` so it lands at
            // the right indent.
            trim_trailing_spaces(out);
            if !out.ends_with('\n') {
                push_newline(out, state);
            }
            state.indent = state.indent.saturating_sub(1);
            push_indent(out, state);
            out.push('}');
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
        "(" | "[" => {
            // No leading space before `(` if we just emitted an ident-ish
            // token (function call, type params).
            if matches!(
                state.last_emitted,
                Some(EmittedKind::CloseBrace) | Some(EmittedKind::Newline) | None
            ) {
                push_indent_if_at_line_start(out, state);
            }
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::LParen);
            state.suppress_space = true;
        }
        ")" | "]" => {
            trim_trailing_spaces(out);
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::RParen);
        }
        "doc_comment" | "line_comment" => {
            push_indent_if_at_line_start(out, state);
            out.push_str(text);
            push_newline(out, state);
            state.last_emitted = Some(EmittedKind::Newline);
        }
        _ => {
            if needs_leading_space(state) {
                out.push(' ');
            }
            push_indent_if_at_line_start(out, state);
            out.push_str(text);
            state.last_emitted = Some(EmittedKind::Other);
            state.suppress_space = false;
        }
    }
    state.last_byte = node.end_byte();
}

fn needs_leading_space(state: &State) -> bool {
    if state.suppress_space {
        return false;
    }
    !matches!(
        state.last_emitted,
        Some(EmittedKind::OpenBrace)
            | Some(EmittedKind::Newline)
            | Some(EmittedKind::LParen)
            | Some(EmittedKind::Dot)
            | Some(EmittedKind::Arrow)
            | None
    )
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
    if matches!(state.last_emitted, Some(EmittedKind::Newline)) {
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
    fn empty_module_is_just_newline() {
        let out = roundtrip("");
        assert_eq!(out, "\n");
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
