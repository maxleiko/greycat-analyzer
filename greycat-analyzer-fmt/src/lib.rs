//! Formatter for `.gcl` source.
//!
//! Three small layers on a Wadler/Leijen pretty printer:
//!
//! - [`doc`] — the layout IR (`Doc::Text`, `Doc::Group`, `Doc::Indent`,
//!   `Doc::Line`, `Doc::Hard`, `Doc::IfBroken`, `Doc::BlankLine`).
//! - [`lower`] — CST → Doc visitor that walks the tree-sitter tree in
//!   named-structure order.
//! - [`render`] — width-aware printer that picks flat-vs-broken per
//!   `Group` via fits-flat measurement.
//!
//! Pragma-driven options ([`FmtOptions`]) are read off the input's
//! `mod_pragma` chain by [`parse_pragma_options`] and applied to the
//! defaults before lowering.

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
/// [`FmtOptions`], overlaid with whatever `@format_*` pragmas the source
/// carries.
pub fn format(source: &str) -> String {
    let opts = parse_pragma_options(source, FmtOptions::default());
    format_with(source, opts)
}

/// Format `source` under explicit options. Pragmas in the source are
/// **not** consulted — caller is responsible for them.
pub fn format_with(source: &str, opts: FmtOptions) -> String {
    let tree = greycat_analyzer_syntax::parse(source);
    let root = tree.root_node();
    if root.has_error() {
        // Mirror the TS reference: a tree with fatal parse errors is
        // returned verbatim.
        return source.to_string();
    }
    format_tree_with(source, root, opts)
}

/// Format a pre-parsed CST under default options. Useful when callers
/// already have a tree from incremental parsing (LSP, editor
/// integrations). Pragmas are consulted from `source`.
pub fn format_tree(source: &str, root: Node<'_>) -> String {
    let opts = parse_pragma_options(source, FmtOptions::default());
    format_tree_with(source, root, opts)
}

/// Format a pre-parsed CST under explicit options. Pragmas are **not**
/// consulted.
pub fn format_tree_with(source: &str, root: Node<'_>, opts: FmtOptions) -> String {
    let cx = lower::Cx::new(source);
    let doc = lower::lower_module(&cx, root);
    render::render(&doc, &opts)
}

/// Parse `@format_line_width(N)` / `@format_indent(N)` /
/// `@format_eol_last(bool)` pragmas off the source's mod_pragma chain
/// and overlay them on `defaults`. Mirrors the TS reference at
/// `parser/cst/cst_format.ts:138-172`.
pub fn parse_pragma_options(source: &str, mut defaults: FmtOptions) -> FmtOptions {
    let tree = greycat_analyzer_syntax::parse(source);
    let root = tree.root_node();
    let mut walker = root.walk();
    for child in root.named_children(&mut walker) {
        if child.kind() != "mod_pragma" {
            continue;
        }
        let mut sub = child.walk();
        for c in child.named_children(&mut sub) {
            if c.kind() != "annotation" {
                continue;
            }
            // annotation: @<ident>(args?)
            let mut ann = c.walk();
            let mut name: Option<&str> = None;
            let mut args_node: Option<Node<'_>> = None;
            for ac in c.named_children(&mut ann) {
                match ac.kind() {
                    "ident" => name = Some(&source[ac.byte_range()]),
                    "args" => args_node = Some(ac),
                    _ => {}
                }
            }
            let Some(name) = name else { continue };
            let Some(args) = args_node else { continue };
            // Single argument expected.
            let mut arg_iter = args.walk();
            let arg = args.named_children(&mut arg_iter).next();
            let Some(arg) = arg else { continue };
            let arg_text = source[arg.byte_range()].trim();
            match name {
                "format_line_width" => {
                    if let Ok(n) = arg_text.parse::<usize>() {
                        defaults.line_width = n;
                    }
                }
                "format_indent" => {
                    if let Ok(n) = arg_text.parse::<usize>() {
                        defaults.indent = n;
                    }
                }
                "format_eol_last" => match arg_text {
                    "true" => defaults.eol_last = true,
                    "false" => defaults.eol_last = false,
                    _ => {}
                },
                _ => {}
            }
        }
    }
    defaults
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

    #[test]
    fn pragma_indent_overrides_default() {
        let src = "@format_indent(2);\nfn f() { var x = 1; }\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert_eq!(opts.indent, 2);
    }

    #[test]
    fn pragma_line_width_overrides_default() {
        let src = "@format_line_width(40);\nfn f() {}\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert_eq!(opts.line_width, 40);
    }

    #[test]
    fn pragma_eol_last_overrides_default() {
        let src = "@format_eol_last(true);\nfn f() {}\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert!(opts.eol_last);
    }

    #[test]
    fn pragma_line_width_drives_break_decision() {
        let src = "@format_line_width(5);\nfn long_name(a: int, b: int) {}\n";
        let out = format(src);
        // Width=5 forces every Group to break; fn_params must split.
        assert!(
            out.contains("\n    a: int,"),
            "expected broken params:\n{out}"
        );
    }

    #[test]
    fn pragma_indent_drives_indent_step() {
        let src = "@format_indent(2);\ntype Foo {\n  a: int;\n}\n";
        let out = format(src);
        assert!(
            out.contains("\n  a: int;"),
            "expected 2-space indent:\n{out}"
        );
    }
}
