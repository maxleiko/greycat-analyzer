//! Width-aware renderer for the Doc IR.
//!
//! Standard Wadler/Leijen "fits-flat" algorithm: for each `Group`, walk
//! the doc once with a notional column budget; if the flat width fits,
//! lay out flat (lines as spaces / nothing); otherwise break (lines as
//! newline + current indent). `Hard` always breaks and counts as
//! infinity in the fits check, so any group containing a `Hard`
//! transitively breaks.
//!
//! The renderer is single-pass after the fits check: no backtracking
//! beyond the lookahead the fits check itself performs.

use crate::FmtOptions;
use crate::doc::Doc;

/// Whether a group/line is being laid out flat or broken.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

/// Render the doc to a string under the given options. Output never
/// contains trailing whitespace on any line; a trailing newline is
/// appended iff `opts.eol_last`.
pub fn render(doc: &Doc, opts: &FmtOptions) -> String {
    let mut out = String::new();
    let mut col: usize = 0;
    let mut stack: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, doc)];
    while let Some((indent, mode, item)) = stack.pop() {
        match item {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(s);
                col += display_width(s);
            }
            Doc::Concat(parts) => {
                for p in parts.iter().rev() {
                    stack.push((indent, mode, p));
                }
            }
            Doc::Indent(inner) => {
                stack.push((indent + opts.indent, mode, inner));
            }
            Doc::Group(inner) => {
                let chosen = if fits(inner, opts.line_width.saturating_sub(col), &stack, opts) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((indent, chosen, inner));
            }
            Doc::Line { space_if_flat } => match mode {
                Mode::Flat => {
                    if *space_if_flat {
                        out.push(' ');
                        col += 1;
                    }
                }
                Mode::Break => {
                    push_newline_with_indent(&mut out, &mut col, indent);
                }
            },
            Doc::Hard => {
                push_newline_with_indent(&mut out, &mut col, indent);
            }
            Doc::BlankLine => {
                push_blank_line(&mut out, &mut col, indent);
            }
            Doc::IfBroken(inner) => {
                if mode == Mode::Break {
                    stack.push((indent, mode, inner));
                }
            }
        }
    }
    finalize(&mut out, opts);
    out
}

/// Decide whether `doc` (laid out flat) fits in `width` columns of the
/// current line. Walks `doc` plus the *remainder of the outer stack*
/// because a group's fit depends on what follows on the same line —
/// once a Hard / Line-in-Break / BlankLine is encountered in the
/// continuation, the line ends and anything after is on a fresh line
/// (so the group fits regardless of trailing content).
fn fits(doc: &Doc, width: usize, outer: &[(usize, Mode, &Doc)], opts: &FmtOptions) -> bool {
    if width == 0 && !is_zero_width_flat(doc) {
        return false;
    }
    let mut budget: isize = width as isize;
    let mut local: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Flat, doc)];
    // True while we're still walking the candidate doc; flips to false
    // once `local` first drains and we start consuming the outer
    // continuation. The distinction matters for hard breaks: a Hard
    // *within* the candidate forces a break (return false), but a Hard
    // *in the continuation* ends the current line (return true) — the
    // candidate fits if it fits up to the next line boundary, period.
    let mut in_candidate = true;
    let mut outer_iter_idx = outer.len();
    loop {
        let (indent, mode, item) = if let Some(top) = local.pop() {
            top
        } else if outer_iter_idx > 0 {
            in_candidate = false;
            outer_iter_idx -= 1;
            outer[outer_iter_idx]
        } else {
            return true;
        };
        match item {
            Doc::Nil => {}
            Doc::Text(s) => {
                budget -= display_width(s) as isize;
                if budget < 0 {
                    return false;
                }
            }
            Doc::Concat(parts) => {
                for p in parts.iter().rev() {
                    local.push((indent, mode, p));
                }
            }
            Doc::Indent(inner) => {
                local.push((indent + opts.indent, mode, inner));
            }
            Doc::Group(inner) => {
                // Nested groups inside the candidate are measured flat
                // (if the outer fits flat, the inner does too). In the
                // continuation, nested groups inherit their parent's
                // already-decided mode — but we don't know that here,
                // so flat is the safe approximation; the caller's own
                // fits-check at render time will give the exact answer.
                local.push((indent, Mode::Flat, inner));
            }
            Doc::Line { space_if_flat } => {
                if !in_candidate {
                    // Continuation Line: in flat mode it's a space,
                    // in break mode it ends the line.
                    match mode {
                        Mode::Flat => {
                            if *space_if_flat {
                                budget -= 1;
                                if budget < 0 {
                                    return false;
                                }
                            }
                        }
                        Mode::Break => return true,
                    }
                } else {
                    // Inside the candidate, we're always in Flat mode
                    // (the candidate is being measured *as if* flat).
                    if *space_if_flat {
                        budget -= 1;
                        if budget < 0 {
                            return false;
                        }
                    }
                }
            }
            Doc::Hard => {
                if in_candidate {
                    // Forces a break inside the candidate — does not
                    // fit flat.
                    return false;
                } else {
                    // Continuation Hard: line ends here, anything past
                    // is on a fresh line — candidate fits.
                    return true;
                }
            }
            Doc::BlankLine => {
                // Always ends the current line.
                return true;
            }
            Doc::IfBroken(_) => {
                // In flat mode, IfBroken contributes nothing.
            }
        }
    }
}

fn is_zero_width_flat(doc: &Doc) -> bool {
    match doc {
        Doc::Nil => true,
        Doc::Line {
            space_if_flat: false,
        } => true,
        Doc::Concat(parts) => parts.iter().all(is_zero_width_flat),
        Doc::Group(inner) | Doc::Indent(inner) => is_zero_width_flat(inner),
        Doc::IfBroken(_) => true,
        _ => false,
    }
}

fn push_newline_with_indent(out: &mut String, col: &mut usize, indent: usize) {
    trim_trailing_spaces(out);
    out.push('\n');
    for _ in 0..indent {
        out.push(' ');
    }
    *col = indent;
}

fn push_blank_line(out: &mut String, col: &mut usize, indent: usize) {
    // Already on a blank line? Coalesce.
    let trailing_blanks = trailing_blank_count(out);
    trim_trailing_spaces(out);
    if trailing_blanks >= 2 {
        // Already have a blank line — no further action.
        return;
    }
    // Make sure we're at a line start, then add exactly one blank.
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    for _ in 0..indent {
        out.push(' ');
    }
    *col = indent;
}

fn trailing_blank_count(s: &str) -> usize {
    let mut count = 0;
    for c in s.chars().rev() {
        if c == '\n' {
            count += 1;
        } else if c == ' ' || c == '\t' {
            continue;
        } else {
            break;
        }
    }
    count
}

fn trim_trailing_spaces(s: &mut String) {
    while matches!(s.chars().next_back(), Some(' ') | Some('\t')) {
        s.pop();
    }
}

fn finalize(out: &mut String, opts: &FmtOptions) {
    // Strip every line's trailing whitespace.
    // (push_newline_with_indent already trims before each newline; we
    // only need to trim the very last line here.)
    while matches!(out.chars().next_back(), Some(' ') | Some('\t')) {
        out.pop();
    }
    if opts.eol_last {
        if !out.ends_with('\n') {
            out.push('\n');
        }
    } else {
        while out.ends_with('\n') {
            out.pop();
        }
    }
}

/// Visible width of `s` in columns. Tabs count as 1 (we don't emit them
/// ourselves, but a comment may carry one); everything else is a byte
/// count for ASCII, falls back to char-count for multibyte.
fn display_width(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FmtOptions {
        FmtOptions {
            line_width: 80,
            indent: 4,
            eol_last: false,
        }
    }

    #[test]
    fn text_renders_verbatim() {
        let d = Doc::text("hello");
        assert_eq!(render(&d, &opts()), "hello");
    }

    #[test]
    fn group_fits_flat() {
        let d = Doc::group(Doc::concat(vec![
            Doc::text("a"),
            Doc::line(),
            Doc::text("b"),
        ]));
        assert_eq!(render(&d, &opts()), "a b");
    }

    #[test]
    fn group_breaks_when_too_wide() {
        let mut o = opts();
        o.line_width = 3;
        let d = Doc::group(Doc::concat(vec![
            Doc::text("aa"),
            Doc::line(),
            Doc::text("bb"),
        ]));
        // 'aa' (2) + ' ' (1) + 'bb' (2) = 5 > 3 → broken.
        assert_eq!(render(&d, &o), "aa\nbb");
    }

    #[test]
    fn indent_applied_on_break() {
        let mut o = opts();
        o.line_width = 3;
        o.indent = 2;
        let d = Doc::group(Doc::concat(vec![
            Doc::text("aa"),
            Doc::indent(Doc::concat(vec![Doc::line(), Doc::text("bb")])),
        ]));
        assert_eq!(render(&d, &o), "aa\n  bb");
    }

    #[test]
    fn hard_breaks_always() {
        let d = Doc::group(Doc::concat(vec![
            Doc::text("a"),
            Doc::hardline(),
            Doc::text("b"),
        ]));
        assert_eq!(render(&d, &opts()), "a\nb");
    }

    #[test]
    fn softline_in_flat_emits_nothing() {
        let d = Doc::group(Doc::concat(vec![
            Doc::text("a"),
            Doc::softline(),
            Doc::text("b"),
        ]));
        assert_eq!(render(&d, &opts()), "ab");
    }

    #[test]
    fn if_broken_renders_only_when_broken() {
        let mut o = opts();
        o.line_width = 3;
        let d = Doc::group(Doc::concat(vec![
            Doc::text("aa"),
            Doc::line(),
            Doc::text("bb"),
            Doc::if_broken(Doc::text(",")),
        ]));
        // Broken: 'aa\nbb,'
        assert_eq!(render(&d, &o), "aa\nbb,");
        // Flat: 'aa bb' (no trailing comma)
        let mut wide = opts();
        wide.line_width = 80;
        assert_eq!(render(&d, &wide), "aa bb");
    }

    #[test]
    fn blank_line_emits_one_blank_between_text() {
        let d = Doc::concat(vec![
            Doc::text("a"),
            Doc::hardline(),
            Doc::blank_line(),
            Doc::text("b"),
        ]);
        assert_eq!(render(&d, &opts()), "a\n\nb");
    }

    #[test]
    fn blank_line_coalesces_when_repeated() {
        let d = Doc::concat(vec![
            Doc::text("a"),
            Doc::hardline(),
            Doc::blank_line(),
            Doc::blank_line(),
            Doc::text("b"),
        ]);
        assert_eq!(render(&d, &opts()), "a\n\nb");
    }

    #[test]
    fn eol_last_appends_trailing_newline() {
        let mut o = opts();
        o.eol_last = true;
        assert_eq!(render(&Doc::text("a"), &o), "a\n");
    }

    #[test]
    fn no_trailing_whitespace_on_any_line() {
        // After indent on a break, the indent contributes spaces but the
        // *next* doc supplies content. If the next doc would be empty
        // (e.g. an empty Indent block), we shouldn't leave the indent
        // hanging.
        let d = Doc::concat(vec![
            Doc::text("{"),
            Doc::indent(Doc::concat(vec![Doc::hardline(), Doc::text("body")])),
            Doc::hardline(),
            Doc::text("}"),
        ]);
        let out = render(&d, &opts());
        for line in out.lines() {
            assert_eq!(
                line.trim_end_matches([' ', '\t']),
                line,
                "trailing whitespace on line: {line:?}\nfull: {out:?}"
            );
        }
    }

    #[test]
    fn nested_group_inherits_break_when_outer_breaks_via_hard() {
        // Outer group has a Hard somewhere → must break → inner Line
        // becomes a newline.
        let d = Doc::group(Doc::concat(vec![
            Doc::text("a"),
            Doc::line(),
            Doc::text("b"),
            Doc::hardline(),
            Doc::text("c"),
        ]));
        assert_eq!(render(&d, &opts()), "a\nb\nc");
    }
}
