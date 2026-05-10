//! Source-gap trivia scanner.
//!
//! The tree-sitter grammar exposes `line_comment` and `doc_comment` as
//! named tree nodes — the lowering visitor reaches them via the tree
//! walk. `_block_comment` is a *hidden* extra (the leading `_` makes it
//! invisible in the CST), so a `/* ... */` token leaves no trace in the
//! tree. Whitespace likewise.
//!
//! This scanner walks the source between two known byte offsets and
//! yields the **gap-only** trivia: block comments and newline-count
//! markers. The lowering uses it to detect blank-line breaks and to
//! re-attach inline `/* */` comments that the tree dropped.

use std::ops::Range;

#[derive(Debug, Clone)]
pub enum GapItem<'a> {
    /// `n` source newline characters in this run, no comment between.
    /// `n >= 1` always; `n >= 2` means a blank line.
    Newlines(u32),
    /// A `/* ... */` block comment encountered in the gap. Multi-line
    /// block comments are passed through verbatim (the formatter is
    /// not in the business of reflowing comment text).
    BlockComment(&'a str),
}

/// Scan `source[range]` for block comments + newline runs, in source
/// order. Whitespace other than newlines is collapsed (we don't care
/// about user-inserted spaces in a gap — the formatter decides spacing).
pub fn scan_gap(source: &str, range: Range<usize>) -> Vec<GapItem<'_>> {
    let bytes = source.as_bytes();
    let end = range.end.min(bytes.len());
    let mut i = range.start.min(end);
    let mut out: Vec<GapItem<'_>> = Vec::new();
    let mut nl_run: u32 = 0;
    while i < end {
        let b = bytes[i];
        if b == b'\n' {
            nl_run += 1;
            i += 1;
            continue;
        }
        if b == b' ' || b == b'\t' || b == b'\r' {
            i += 1;
            continue;
        }
        if nl_run > 0 {
            out.push(GapItem::Newlines(nl_run));
            nl_run = 0;
        }
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'*' {
            let cmt_start = i;
            i += 2;
            // Find the matching `*/` (the grammar guarantees one).
            while i + 1 < end {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            let cmt = &source[cmt_start..i];
            out.push(GapItem::BlockComment(cmt));
            continue;
        }
        // `//` line comments would appear here if the tree didn't
        // already surface them. Skip the run defensively in case the
        // grammar's extra-rule emission rules ever change.
        if b == b'/' && i + 1 < end && bytes[i + 1] == b'/' {
            while i < end && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Any other byte means the gap is over (we hit a non-trivia
        // byte that the caller's range shouldn't have included). Bail
        // gracefully so a misuse can't panic.
        break;
    }
    if nl_run > 0 {
        out.push(GapItem::Newlines(nl_run));
    }
    out
}

/// Count the number of `\n` bytes in `source[range]`. Cheap helper for
/// the common "did the user have a blank line here?" question without
/// allocating a full `Vec<GapItem>`.
pub fn newline_count(source: &str, range: Range<usize>) -> u32 {
    let bytes = source.as_bytes();
    let end = range.end.min(bytes.len());
    let start = range.start.min(end);
    bytes[start..end].iter().filter(|&&b| b == b'\n').count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_gap_returns_empty() {
        assert!(scan_gap("hello", 5..5).is_empty());
    }

    #[test]
    fn newline_run_collapses_to_one_item() {
        let s = "a\n\n\nb";
        let items = scan_gap(s, 1..4);
        assert_eq!(items.len(), 1);
        match items[0] {
            GapItem::Newlines(n) => assert_eq!(n, 3),
            _ => panic!("{items:?}"),
        }
    }

    #[test]
    fn whitespace_only_yields_nothing() {
        let items = scan_gap("a   b", 1..4);
        assert!(items.is_empty());
    }

    #[test]
    fn block_comment_round_trips_verbatim() {
        let s = "a /* hi */ b";
        let items = scan_gap(s, 1..10);
        assert_eq!(items.len(), 1);
        match &items[0] {
            GapItem::BlockComment(t) => assert_eq!(*t, "/* hi */"),
            _ => panic!("{items:?}"),
        }
    }

    #[test]
    fn newlines_around_block_comment_are_distinct_items() {
        let s = "a\n/* hi */\n\nb";
        // Gap starts after 'a' (byte 1) and ends before 'b' (byte 12).
        let items = scan_gap(s, 1..12);
        assert!(matches!(items[0], GapItem::Newlines(1)));
        assert!(matches!(items[1], GapItem::BlockComment("/* hi */")));
        assert!(matches!(items[2], GapItem::Newlines(2)));
    }

    #[test]
    fn newline_count_counts_only_newlines() {
        assert_eq!(newline_count("a\n\n\nb", 1..4), 3);
        assert_eq!(newline_count("abc", 0..3), 0);
        assert_eq!(newline_count("a\rb", 0..3), 0);
    }
}
