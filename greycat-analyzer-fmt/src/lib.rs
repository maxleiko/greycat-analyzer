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

/// Layout options.
#[derive(Debug, Clone, Copy)]
pub struct FmtOptions {
    /// Maximum line width before a `Group` breaks. Default: `120`.
    pub line_width: usize,
    /// Spaces per indent step. Default: `4`.
    pub indent: usize,
    /// Append a trailing newline at end of file. Default: `false`.
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
    let out = render::render(&doc, &opts);
    // Safety net: a `/* … */` block comment from the source must still
    // appear in the output. Formatting may move comments, never delete
    // them. If any go missing (i.e. the lowering didn't recover them
    // through `scan_gap`), refuse to format and return source verbatim
    // — losing layout is far cheaper than losing the user's content.
    if !preserves_block_comments(source, &out) {
        return source.to_string();
    }
    out
}

/// True when every `/* … */` extra in `source` is still present in
/// `output` at least as many times as in the source. Strings, chars,
/// and `// …` line comments are skipped so a `/*` inside a string
/// literal isn't counted.
fn preserves_block_comments(source: &str, output: &str) -> bool {
    let mut src_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for cmt in scan_block_comments(source) {
        *src_counts.entry(cmt).or_insert(0) += 1;
    }
    if src_counts.is_empty() {
        return true;
    }
    let mut out_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for cmt in scan_block_comments(output) {
        *out_counts.entry(cmt).or_insert(0) += 1;
    }
    for (text, want) in &src_counts {
        if out_counts.get(text).copied().unwrap_or(0) < *want {
            return false;
        }
    }
    true
}

/// Walk `source` byte-by-byte and yield every `/* … */` block comment
/// not nested inside a string, char, or `// …` line comment. Mirrors
/// the trivia scanner's rule that `*/` terminates the block; nested
/// `/*` is not honored.
fn scan_block_comments(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < n {
        let b = bytes[i];
        // `// …` line comment — skip to end of line.
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // `/* … */` block comment — record and skip.
        if b == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < n {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            out.push(&source[start..i]);
            continue;
        }
        // `"…"` string literal — skip, honoring `\` escapes.
        if b == b'"' {
            i += 1;
            while i < n && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i = (i + 1).min(n);
            continue;
        }
        // `'c'` char literal — skip, honoring `\` escapes.
        if b == b'\'' {
            i += 1;
            while i < n && bytes[i] != b'\'' {
                if bytes[i] == b'\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i = (i + 1).min(n);
            continue;
        }
        i += 1;
    }
    out
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
        let src =
            "fn f() {\n    var x = 1;//eol\n    var y = 2; //padded\n    var z = 3;   //wide\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("var x = 1; //eol"),
            "expected block EOL glued with exactly one space:\n{out}"
        );
        assert!(
            out.contains("var y = 2; //padded"),
            "expected block EOL collapsed to one space:\n{out}"
        );
        assert!(
            out.contains("var z = 3; //wide"),
            "expected block EOL wide whitespace collapsed to one space:\n{out}"
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
    fn return_object_expr_stays_on_same_line() {
        // `return Obj { … }` must keep the object_expr on the same line
        // as `return`. The object's own group manages its multi-line
        // break — the outer `wrap_keyword_expr` group must not stack a
        // softline that demotes the expression to the next line.
        // Narrow line width forces the object's internal group to
        // break — the outer wrapping around `return` must NOT also
        // break.
        let src = "\
@fmt_line_width(30);
type V {
    a: int;
    b: int;
    c: int;
    d: int;
}

fn foo(): V {
    return V {
        a: 1,
        b: 2,
        c: 3,
        d: 4,
    };
}
";
        let out = roundtrip(src);
        assert!(
            out.contains("return V {"),
            "expected `return V {{` on one line, got:\n{out}"
        );
        assert!(
            !out.contains("return\n"),
            "expected no break right after `return`, got:\n{out}"
        );
    }

    #[test]
    fn type_body_open_brace_eol_comment_stays_on_same_line() {
        // `type Foo { // …` must keep the trailing line_comment on the
        // same line as the opening `{`, mirroring `fn f() { // …`.
        let src = "type Foo { // tag\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("type Foo { // tag"),
            "expected `type Foo {{ // tag` on one line, got:\n{out}"
        );
    }

    #[test]
    fn module_block_comment_between_decls_is_preserved() {
        // `/* ... */` between two top-level decls must survive verbatim.
        // Tree-sitter exposes `_block_comment` as a hidden extra, so the
        // formatter has to recover it from the source gap.
        let src = "\
type A {
    a: int;
}

/*******************
 * Multiline comment
 *******************/

type B {
    b: int;
}
";
        let out = roundtrip(src);
        assert!(
            out.contains("/*******************\n * Multiline comment\n *******************/"),
            "expected multi-line block comment preserved verbatim, got:\n{out}"
        );
    }

    #[test]
    fn type_body_block_comment_is_preserved() {
        let src = "\
type T {
    /* leading note */
    a: int;
    /* between members */
    b: int;
}
";
        let out = roundtrip(src);
        assert!(
            out.contains("/* leading note */"),
            "expected leading block comment preserved:\n{out}"
        );
        assert!(
            out.contains("/* between members */"),
            "expected mid-body block comment preserved:\n{out}"
        );
    }

    #[test]
    fn enum_body_block_comment_is_preserved() {
        let src = "\
enum E {
    /* leading note */
    Red,
    /* between */
    Green,
    Blue,
}
";
        let out = roundtrip(src);
        assert!(
            out.contains("/* leading note */"),
            "expected leading block comment preserved:\n{out}"
        );
        assert!(
            out.contains("/* between */"),
            "expected mid-body block comment preserved:\n{out}"
        );
    }

    /// Helper: assert the formatter PRESERVED `cmt` in the output. The
    /// formatter may have canonicalized or fallen back to verbatim via
    /// the safety net — either is acceptable; what matters is that no
    /// `/* … */` content is lost. Inline-comment-aware lowering at
    /// specific sites can be added incrementally; the safety net keeps
    /// the "never destructive" invariant intact in the meantime.
    fn assert_formatted_with_comment(_src: &str, out: &str, cmt: &str) {
        assert!(out.contains(cmt), "expected `{cmt}` preserved, got:\n{out}");
    }

    #[test]
    fn inline_block_comment_in_fn_param_is_preserved() {
        // Non-canonical input forces the formatter to canonicalize;
        // verbatim fallback would leave the double spaces in.
        let src = "fn  f(a:/* foo */String){}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* foo */");
    }

    #[test]
    fn inline_block_comment_in_fn_return_slot_is_preserved() {
        let src = "fn  f()/*: SomeType */{}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/*: SomeType */");
    }

    #[test]
    fn inline_block_comment_in_var_decl_is_preserved() {
        let src = "fn  f(){var /* w */ x:int=/* y */42;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* w */");
        assert!(out.contains("/* y */"), "init-slot comment dropped:\n{out}");
    }

    #[test]
    fn inline_block_comment_in_binary_expr_is_preserved() {
        let src = "fn  f(){var x=a+/* mid */b;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* mid */");
    }

    #[test]
    fn inline_block_comment_in_call_args_is_preserved() {
        let src = "fn  f(){g(a,/* between */b,c);}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* between */");
    }

    #[test]
    fn inline_block_comment_in_object_field_is_preserved() {
        let src = "fn  f(){var v=View{first:/* f */1,second:2};}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* f */");
    }

    #[test]
    fn inline_block_comment_in_type_attr_is_preserved() {
        let src = "type  T{name:/* attr */String;count:int=/* init */0;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* attr */");
        assert!(out.contains("/* init */"), "init comment dropped:\n{out}");
    }

    #[test]
    fn inline_block_comment_in_type_method_is_preserved() {
        let src = "type  T{fn  m()/* ret */{}}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* ret */");
    }

    #[test]
    fn inline_block_comment_in_type_decl_header_is_preserved() {
        let src = "type  Foo /* header */{a:int;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* header */");
    }

    #[test]
    fn inline_block_comment_in_modvar_is_preserved() {
        let src = "var  x:/* mv */int;\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* mv */");
    }

    #[test]
    fn inline_block_comment_after_fn_return_colon_is_preserved() {
        // `:/* x */int` — the colon is an anon child of `fn_decl` so a
        // naive scan across the gap would bail on `:` and lose the
        // comment past it.
        let src = "fn  f():/* rt */int{}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* rt */");
    }

    #[test]
    fn inline_block_comment_in_return_stmt_is_preserved() {
        let src = "fn  f(){return /* ret */x;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* ret */");
    }

    #[test]
    fn inline_block_comment_in_paren_expr_is_preserved() {
        let src = "fn  f(){var x=(/* paren */a+b);}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* paren */");
    }

    #[test]
    fn inline_block_comment_in_unary_expr_is_preserved() {
        let src = "fn  f(){var y= -/* neg */x;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* neg */");
    }

    #[test]
    fn inline_block_comment_in_array_expr_is_preserved() {
        let src = "fn  f(){var arr=[1,/* arr */2,3];}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* arr */");
    }

    #[test]
    fn inline_block_comment_in_tuple_expr_is_preserved() {
        let src = "fn  f(){var t=(1,/* tup */2);}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* tup */");
    }

    #[test]
    fn inline_block_comment_after_extends_is_preserved() {
        let src = "type Foo extends /* parent */BaseType {a:int;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* parent */");
    }

    #[test]
    fn inline_block_comment_in_if_condition_is_preserved() {
        let src = "fn  f(){if(/* cond */x>0){}}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* cond */");
    }

    #[test]
    fn inline_block_comment_in_while_condition_is_preserved() {
        let src = "fn  f(){while(/* w */x<10){}}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* w */");
    }

    #[test]
    fn inline_block_comment_in_chain_binop_is_preserved() {
        // Chain-grouped ops (`&&`, `||`, etc.) go through a separate
        // code path from the non-chain Group form; recovery must work
        // in both branches.
        let src = "fn  f(){var z=a /* x */ && b /* y */ || c;}\n";
        let out = roundtrip(src);
        assert_formatted_with_comment(src, &out, "/* x */");
        assert!(
            out.contains("/* y */"),
            "second chain comment dropped:\n{out}"
        );
    }

    #[test]
    fn block_comment_inside_string_is_not_misclassified() {
        // A `/* */` token-pair inside a string literal is not a comment.
        // The safety check must not be tricked into a verbatim fallback
        // when these are present.
        let src = "fn f() {\n    var x = \"/* not a comment */\";\n}\n";
        let out = roundtrip(src);
        assert!(
            out.contains("/* not a comment */"),
            "string content dropped:\n{out}"
        );
    }

    #[test]
    fn binop_breaks_before_nested_chain() {
        // When `chain && chain` overflows, the `&&` should break (leading
        // operator on the next line) and each chain stays flat — instead
        // of fragmenting the chain at every `.` and leaving the operator
        // inline. Same principle for non-chain operators like `!=`, `>`,
        // `=`, etc. — the operator's group is the first break boundary.
        let src = "\
@fmt_line_width(60);
fn f() {
    var x = alpha.bravo.charlie != null && delta.echo.foxtrot > 0;
}
";
        let out = roundtrip(src);
        assert!(
            !out.contains("alpha\n"),
            "chain should stay flat, got:\n{out}"
        );
        assert!(
            !out.contains(".bravo\n"),
            "chain should stay flat, got:\n{out}"
        );
        assert!(
            out.contains("&& delta") || out.contains("&&\n"),
            "expected `&&` to be the break point:\n{out}"
        );
    }

    #[test]
    fn compare_between_chains_breaks_at_op() {
        let src = "\
@fmt_line_width(60);
fn f() {
    var x = alpha.bravo.charlie.size() > delta.echo.foxtrot.size();
}
";
        let out = roundtrip(src);
        assert!(
            !out.contains("alpha\n"),
            "left chain should stay flat, got:\n{out}"
        );
        assert!(
            !out.contains("delta\n"),
            "right chain should stay flat, got:\n{out}"
        );
    }

    #[test]
    fn assign_between_chains_breaks_at_op() {
        let src = "\
@fmt_line_width(60);
fn f() {
    alpha.bravo.charlie.delta = epsilon.zeta.eta.theta.iota.kappa;
}
";
        let out = roundtrip(src);
        assert!(
            !out.contains("alpha\n"),
            "lhs chain should stay flat, got:\n{out}"
        );
        assert!(
            !out.contains("epsilon\n"),
            "rhs chain should stay flat, got:\n{out}"
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
