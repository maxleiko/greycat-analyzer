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
pub mod lower;
pub mod render;
pub mod trivia;

use greycat_analyzer_syntax::tree_sitter::Node;

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

/// Format `source` and return the canonical text under the default
/// [`FmtOptions`]. A best-effort pass — see the module docs for parity
/// caveats.
pub fn format(source: &str) -> String {
    format_with(source, FmtOptions::default())
}

/// Format `source` under explicit options.
pub fn format_with(source: &str, opts: FmtOptions) -> String {
    let tree = greycat_analyzer_syntax::parse(source);
    let root = tree.root_node();
    if root.has_error() {
        // Mirror the TS reference: a tree with fatal parse errors is
        // returned verbatim. The legacy walker happened to be tolerant
        // of recoverable errors and produced *something*; the new
        // pipeline doesn't have that fallback yet so we play safe.
        return source.to_string();
    }
    format_tree_with(source, root, opts)
}

/// Format a pre-parsed CST under default options. Useful when callers
/// already have a tree from incremental parsing (LSP, editor
/// integrations).
pub fn format_tree(source: &str, root: Node<'_>) -> String {
    format_tree_with(source, root, FmtOptions::default())
}

/// Format a pre-parsed CST under explicit options.
pub fn format_tree_with(source: &str, root: Node<'_>, opts: FmtOptions) -> String {
    let cx = lower::Cx::new(source);
    let doc = lower::lower_module(&cx, root);
    render::render(&doc, &opts)
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
