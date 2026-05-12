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

pub mod directives;
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
/// [`FmtOptions`], overlaid with whatever `@fmt_*` pragmas the source
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

// P23.4
/// `// gcl-fmt-file-off` short-circuit. When the source's
/// module head carries this directive, return `source.to_string()`
/// without lowering. Centralized here so every public entry point
/// honors it.
fn fmt_off_file(source: &str, root: greycat_analyzer_syntax::tree_sitter::Node<'_>) -> bool {
    directives::FmtDirectives::parse(source, root).fmt_off_file
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
    if fmt_off_file(source, root) {
        return source.to_string();
    }
    let directives = directives::FmtDirectives::parse(source, root);
    let cx = lower::Cx::with_directives(source, directives);
    let doc = lower::lower_module(&cx, root);
    render::render(&doc, &opts)
}

/// Parse `@fmt_line_width(N)` / `@fmt_indent(N)` /
/// `@fmt_eol_last(bool)` pragmas off the source's mod_pragma chain
/// and overlay them on `defaults`. Mirrors the TS reference at
/// `parser/cst/cst_format.ts:138-172` (the TS port still uses
/// `@format_*`; the Rust port normalized to `@fmt_*` in P39.2 so all
/// formatter-touching pragmas share one prefix with the
/// `// gcl-fmt-*` comment-directive family).
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
                "fmt_line_width" => {
                    if let Ok(n) = arg_text.parse::<usize>() {
                        defaults.line_width = n;
                    }
                }
                "fmt_indent" => {
                    if let Ok(n) = arg_text.parse::<usize>() {
                        defaults.indent = n;
                    }
                }
                "fmt_eol_last" => match arg_text {
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
        let src = "@fmt_indent(2);\nfn f() { var x = 1; }\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert_eq!(opts.indent, 2);
    }

    #[test]
    fn pragma_line_width_overrides_default() {
        let src = "@fmt_line_width(40);\nfn f() {}\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert_eq!(opts.line_width, 40);
    }

    #[test]
    fn pragma_eol_last_overrides_default() {
        let src = "@fmt_eol_last(true);\nfn f() {}\n";
        let opts = parse_pragma_options(src, FmtOptions::default());
        assert!(opts.eol_last);
    }

    #[test]
    fn pragma_line_width_drives_break_decision() {
        let src = "@fmt_line_width(5);\nfn long_name(a: int, b: int) {}\n";
        let out = format(src);
        // Width=5 forces every Group to break; fn_params must split.
        assert!(
            out.contains("\n    a: int,"),
            "expected broken params:\n{out}"
        );
    }

    // -----------------------------------------------------------------
    // P23.4 — formatter skip directives
    // -----------------------------------------------------------------

    #[test]
    fn fmt_off_file_short_circuits_to_verbatim() {
        // `// gcl-fmt-file-off` at module head: the formatter must
        // return the source unchanged, even when it'd otherwise
        // reformat heavily.
        let src = "// gcl-fmt-file-off\nfn  weirdly_spaced(  a:int  ){ return  a;  }\n";
        let out = format(src);
        assert_eq!(out, src);
    }

    #[test]
    fn fmt_off_on_pair_preserves_region_verbatim() {
        // The fn between `gcl-fmt-off` / `gcl-fmt-on` keeps its odd
        // spacing; the fn outside the toggle is reformatted normally.
        let src = "\
// gcl-fmt-off
fn  weird(  a:int  ){ return  a;  }
// gcl-fmt-on
fn normal(a: int): int { return a; }
";
        let out = format(src);
        assert!(
            out.contains("fn  weird(  a:int  ){ return  a;  }"),
            "expected verbatim region preserved, got:\n{out}"
        );
    }

    #[test]
    fn fmt_skip_preserves_next_node_verbatim() {
        let src = "\
// gcl-fmt-skip
fn  weird(  a:int  ){ return  a;  }
fn normal(a: int): int { return a; }
";
        let out = format(src);
        assert!(
            out.contains("fn  weird(  a:int  ){ return  a;  }"),
            "expected fmt-skip to preserve next decl verbatim, got:\n{out}"
        );
    }

    #[test]
    fn pragma_indent_drives_indent_step() {
        let src = "@fmt_indent(2);\ntype Foo {\n  a: int;\n}\n";
        let out = format(src);
        assert!(
            out.contains("\n  a: int;"),
            "expected 2-space indent:\n{out}"
        );
    }

    // -----------------------------------------------------------------
    // EOL `//` line-comment placement (must stay on the same source
    // line as the previous member / stmt; spacing-before-`//` matches
    // the TS reference's `cst_format.ts`: one space inside type / enum
    // bodies, zero inside fn / block bodies).
    // -----------------------------------------------------------------

    #[test]
    fn type_attr_eol_comment_stays_on_same_line() {
        let src = "type Example {\n    aaa: String;//tight\n    bbb: int;   // padded\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("aaa: String; //tight"),
            "expected EOL glued with one space:\n{out}"
        );
        assert!(
            out.contains("bbb: int; // padded"),
            "expected leading whitespace before EOL collapsed to one space:\n{out}"
        );
        assert!(
            !out.contains("\n    //tight"),
            "EOL comment must not be demoted to next line:\n{out}"
        );
    }

    #[test]
    fn enum_field_eol_comment_stays_on_same_line() {
        let src = "enum Color {\n    Red,//eol\n    Green,\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("Red, //eol"),
            "expected enum EOL glued with one space after the comma:\n{out}"
        );
    }

    #[test]
    fn block_stmt_eol_comment_stays_on_same_line() {
        let src = "fn f() {\n    var x = 1;//eol\n    var y = 2; //padded\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("var x = 1;//eol"),
            "expected block EOL glued with no leading space:\n{out}"
        );
        assert!(
            out.contains("var y = 2;//padded"),
            "expected block EOL whitespace-before-`//` stripped:\n{out}"
        );
    }

    #[test]
    fn block_open_brace_eol_comment_keeps_one_space() {
        let src = "fn f() {//tight\n    var x = 1;\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("fn f() { //tight"),
            "expected `{{ //` open with one space:\n{out}"
        );
    }

    #[test]
    fn eol_comment_format_is_idempotent() {
        let src = "type T {\n    a: int;//x\n    b: int; // y\n}\n";
        let once = format(src);
        let twice = format(&once);
        assert_eq!(
            once, twice,
            "expected idempotency, got:\n{once}\n--\n{twice}"
        );
    }
}
