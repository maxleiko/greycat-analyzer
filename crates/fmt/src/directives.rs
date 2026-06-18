// P23.4
//! Formatter directive scanner.
//!
//! Walks the CST for `// gcl-fmt-…` line comments and produces a
//! [`FmtDirectives`] table that the lowerer consults to skip nodes
//! whose source bytes should be emitted verbatim.
//!
//! This is the formatter's own copy of the directive parser. The
//! analyzer crate ([`greycat_analyzer_analysis::directives`]) parses
//! the same directives plus the `gcl-lint-…` family for the linter;
//! we duplicate the small fmt-only slice here so the formatter doesn't
//! pull a dependency on analysis (the workspace dependency direction
//! is `analysis → fmt`, not the reverse).

use std::ops::Range;

use greycat_analyzer_syntax::tree_sitter::{Node, TreeCursor};

/// Source byte ranges the formatter must preserve verbatim.
#[derive(Debug, Default, Clone)]
pub struct FmtDirectives {
    /// Ranges to emit verbatim. May overlap (a `gcl-fmt-skip` inside a
    /// `gcl-fmt-off`/`gcl-fmt-on` block, etc.); membership is checked
    /// with [`Self::is_skipped`].
    pub skip_ranges: Vec<Range<usize>>,
    /// `true` when a `gcl-fmt-file-off` was seen at module head — the
    /// caller should emit `source.to_string()` directly without lowering.
    pub fmt_off_file: bool,
}

impl FmtDirectives {
    /// Empty placeholder.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse the CST for fmt directives.
    pub fn parse(source: &str, root: Node<'_>) -> Self {
        let mut comments: Vec<RawComment<'_>> = Vec::new();
        {
            let mut cursor = root.walk();
            walk_comments(&mut cursor, source, &mut comments);
        }

        let source_end = source.len();
        let mut skip_ranges: Vec<Range<usize>> = Vec::new();
        let mut fmt_off_file = false;
        let mut open: Option<usize> = None; // start byte of the open `-off`

        for raw in comments {
            let trimmed = raw.text.strip_prefix("//").unwrap_or(raw.text).trim();
            match trimmed {
                "gcl-fmt-off" if open.is_none() => {
                    open = Some(raw.byte_range.end);
                }
                "gcl-fmt-on" => {
                    if let Some(start) = open.take() {
                        skip_ranges.push(start..raw.byte_range.start);
                    }
                }
                "gcl-fmt-skip" => {
                    if let Some(next) = next_ast_item_range(raw.node) {
                        skip_ranges.push(next);
                    }
                }
                "gcl-fmt-file-off" if is_at_module_head(raw.node) => {
                    fmt_off_file = true;
                }
                _ => {}
            }
        }

        // Unbalanced `gcl-fmt-off` extends to EOF (matches the
        // analyzer-side parser). Warning surfaces from the analyzer's
        // version of the parser; the formatter just honors the range.
        if let Some(start) = open {
            skip_ranges.push(start..source_end);
        }

        FmtDirectives {
            skip_ranges,
            fmt_off_file,
        }
    }

    /// `true` when the byte range is fully inside any skip range.
    pub fn is_skipped(&self, byte_range: &Range<usize>) -> bool {
        self.skip_ranges
            .iter()
            .any(|s| s.start <= byte_range.start && byte_range.end <= s.end)
    }
}

#[derive(Debug)]
struct RawComment<'a> {
    text: &'a str,
    byte_range: Range<usize>,
    node: Node<'a>,
}

fn walk_comments<'a>(cursor: &mut TreeCursor<'a>, source: &'a str, out: &mut Vec<RawComment<'a>>) {
    let n = cursor.node();
    if n.kind() == "line_comment" {
        let r = n.byte_range();
        out.push(RawComment {
            text: &source[r.clone()],
            byte_range: r,
            node: n,
        });
    }
    if cursor.goto_first_child() {
        loop {
            walk_comments(cursor, source, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn next_ast_item_range(comment_node: Node<'_>) -> Option<Range<usize>> {
    let mut node = comment_node;
    loop {
        if let Some(sib) = next_named_non_comment_sibling(node) {
            return Some(sib.byte_range());
        }
        node = node.parent()?;
        if node.kind() == "module" {
            return None;
        }
    }
}

fn next_named_non_comment_sibling<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut sib = node.next_named_sibling();
    while let Some(s) = sib {
        if !matches!(s.kind(), "line_comment" | "doc_comment") {
            return Some(s);
        }
        sib = s.next_named_sibling();
    }
    None
}

fn is_at_module_head(comment_node: Node<'_>) -> bool {
    let Some(parent) = comment_node.parent() else {
        return false;
    };
    if parent.kind() != "module" {
        return false;
    }
    let mut cursor = parent.walk();
    for sib in parent.named_children(&mut cursor) {
        if sib.id() == comment_node.id() {
            return true;
        }
        if !matches!(sib.kind(), "line_comment" | "doc_comment") {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_syntax::parse;

    #[test]
    fn fmt_off_file_at_module_head_sets_flag() {
        let src = "// gcl-fmt-file-off\nfn foo() {}\n";
        let tree = parse(src);
        let d = FmtDirectives::parse(src, tree.root_node());
        assert!(d.fmt_off_file);
    }

    #[test]
    fn fmt_off_file_after_decl_is_ignored() {
        let src = "fn x() {}\n// gcl-fmt-file-off\n";
        let tree = parse(src);
        let d = FmtDirectives::parse(src, tree.root_node());
        assert!(!d.fmt_off_file);
    }

    #[test]
    fn fmt_off_on_pair_records_range() {
        let src = "// gcl-fmt-off\nfn foo() {}\n// gcl-fmt-on\n";
        let tree = parse(src);
        let d = FmtDirectives::parse(src, tree.root_node());
        assert_eq!(d.skip_ranges.len(), 1);
        let foo_start = src.find("fn foo").unwrap();
        let foo_end = src.find("// gcl-fmt-on").unwrap();
        assert!(d.is_skipped(&(foo_start..foo_end)));
    }

    #[test]
    fn fmt_skip_covers_next_node() {
        let src = "// gcl-fmt-skip\nfn foo() {}\n";
        let tree = parse(src);
        let d = FmtDirectives::parse(src, tree.root_node());
        assert_eq!(d.skip_ranges.len(), 1);
        let foo_range = src.find("fn foo").unwrap()..src.len() - 1;
        assert!(d.is_skipped(&foo_range));
    }

    #[test]
    fn unbalanced_fmt_off_extends_to_eof() {
        let src = "// gcl-fmt-off\nfn foo() {}\n";
        let tree = parse(src);
        let d = FmtDirectives::parse(src, tree.root_node());
        assert_eq!(d.skip_ranges.len(), 1);
        let foo_range = src.find("fn foo").unwrap()..src.len();
        assert!(d.is_skipped(&foo_range));
    }
}
