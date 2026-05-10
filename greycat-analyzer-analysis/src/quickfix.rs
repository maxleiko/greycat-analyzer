//! Per-diagnostic auto-fix synthesis.
//!
//! Single source of truth for "given a diagnostic on this source, what
//! edits would make it go away?" — consumed by both the CLI's
//! `lint --fix` driver and the LSP's `textDocument/codeAction` handler.
//! Before P22.7 these lived as parallel implementations in each
//! caller; the duplication was the dominant source of "fix in one
//! place, forget the other" bugs.
//!
//! The fix functions are byte-range based. Callers that work in LSP
//! `Position` space convert at the boundary — that conversion is not
//! the quickfix module's concern.
//!
//! All fix functions return `Vec<TextEdit>`; an empty Vec means "this
//! diagnostic has no automatic fix" (or its preconditions don't hold —
//! see [`unused_param_fix`]'s safety check).

use std::ops::Range;

use crate::actions::TextEdit;
use greycat_analyzer_syntax::tree_sitter::Node;

/// Compute the auto-fix edits for `diag` against `text`. Returns an
/// empty Vec when the rule has no fix or its preconditions don't hold.
pub fn edit_for_diagnostic(
    text: &str,
    code: &str,
    byte_range: &Range<usize>,
    message: &str,
) -> Vec<TextEdit> {
    let start = byte_range.start;
    let end = byte_range.end;
    if end > text.len() || start > end {
        return Vec::new();
    }
    match code {
        "missing-token" => missing_token_fix(start, message),
        "unused-local" => unused_local_fix(text, start),
        "unused-decl" => unused_decl_fix(text, start),
        "unused-param" => unused_param_fix(text, start, end),
        "possibly-null" => possibly_null_fix(text, end),
        "redundant-nullable-access" => redundant_nullable_access_fix(text, start, end),
        "redundant-non-null-assertion" | "redundant-coalesce" => redundant_slice_fix(start, end),
        "modvar-node-cannot-be-nullable" => modvar_strip_outer_nullable_fix(text, end),
        "modvar-node-inner-must-be-nullable" => modvar_append_inner_nullable_fix(end),
        "unused-suppression" => unused_suppression_fix(text, start, end),
        "empty-suppression" | "unbalanced-lint-off" | "unbalanced-fmt-off" => {
            delete_comment_line_fix(text, start)
        }
        "unreachable" => unreachable_fix(text, start, end),
        _ => Vec::new(),
    }
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

/// **P22.1** — replace the **whole** `var x = expr;` statement, not just
/// the ident. The diagnostic's range covers the ident only (for cursor
/// placement); we widen the fix range to the enclosing `var_decl`
/// node by re-parsing and walking up the CST.
fn unused_local_fix(text: &str, ident_start: usize) -> Vec<TextEdit> {
    let Some(stmt_range) = enclosing_node_range(text, ident_start, &["var_decl"]) else {
        return Vec::new();
    };
    vec![TextEdit {
        byte_range: stmt_range,
        new_text: String::new(),
    }]
}

/// **P22.2** — same shape for top-level decls. Walks to the enclosing
/// `fn_decl` / `type_decl` / `enum_decl` / `modvar` and returns its
/// full byte range. Doc comments + annotations sitting immediately
/// above the decl are absorbed (the grammar makes them children of
/// the decl, so the decl's `byte_range` already covers them).
fn unused_decl_fix(text: &str, ident_start: usize) -> Vec<TextEdit> {
    let Some(decl_range) = enclosing_node_range(
        text,
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

/// **P22.3** — rename `name` to `_name` only when the body has zero
/// text-level occurrences of `name`. If the body references the name
/// (which a correctly-detected unused param shouldn't have, but a lint
/// false-positive *might*), refuse the fix. Belt-and-suspenders so a
/// lint detection bug doesn't dangling-bind via auto-fix.
fn unused_param_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    if end <= start {
        return Vec::new();
    }
    let name = &text[start..end];
    if name.starts_with('_') {
        return Vec::new();
    }
    if !is_param_name_unused_in_enclosing_fn(text, start, name) {
        return Vec::new();
    }
    vec![TextEdit {
        byte_range: start..end,
        new_text: format!("_{name}"),
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
/// **P24.6** — fix for `unreachable`. The diagnostic's byte range is
/// already the dead island (single statement, coalesced sibling run,
/// or trailing `else { … }` block). Default: delete that range.
///
/// Special case for the dead-`else` shape: when the dead range starts
/// at a `{` (the body of a final else under exhaustive coverage), walk
/// back over whitespace to find and swallow the leading `else` keyword
/// alongside any whitespace between them. Otherwise we'd leave
/// `if (…) { … } else ` dangling — a parse error.
fn unreachable_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let bytes = text.as_bytes();
    if end > bytes.len() || start > end {
        return Vec::new();
    }
    let mut del_start = start;
    // Detect the "dead else block" shape: range starts at a `{`.
    if bytes.get(start) == Some(&b'{') {
        let mut i = start;
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        // Look back for "else" — a 4-byte ASCII keyword preceded by a
        // word boundary and followed by whitespace (which we just
        // walked over).
        if i >= 4 && &bytes[i - 4..i] == b"else" {
            let kw_start = i - 4;
            let pre_ok = kw_start == 0
                || !matches!(
                    bytes[kw_start - 1],
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'
                );
            if pre_ok {
                // Swallow leading whitespace before the `else` too,
                // so we don't leave a trailing space after the prior
                // `}`. Stop at a newline so the prior block's
                // indentation isn't disturbed.
                let mut j = kw_start;
                while j > 0 && matches!(bytes[j - 1], b' ' | b'\t') {
                    j -= 1;
                }
                del_start = j;
            }
        }
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

/// **P23.3 follow-up** — fix for `unused-suppression`. The diagnostic's
/// `byte_range` points at the dead rule word inside a `// gcl-lint-…`
/// directive comment. Two shapes:
///
/// - **Multi-rule directive** (`// gcl-lint-off-next A B`, B is dead):
///   delete `B` plus its leading whitespace separator → leaves
///   `// gcl-lint-off-next A`. If `B` was the *first* rule, eat the
///   trailing whitespace instead so the result is `// gcl-lint-off-next
///   …rest…`.
/// - **Sole rule** (`// gcl-lint-off-next B`, B is dead): the directive
///   becomes useless when its only rule is removed; delete the entire
///   comment line (including any leading whitespace and the trailing
///   newline if the comment was the only content on the line).
///
/// Returns an empty Vec when the diagnostic byte range doesn't sit
/// inside a `line_comment`'s rule-list slot.
fn unused_suppression_fix(text: &str, start: usize, end: usize) -> Vec<TextEdit> {
    let Some(comment_range) = enclosing_node_range(text, start, &["line_comment"]) else {
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
fn delete_comment_line_fix(text: &str, byte: usize) -> Vec<TextEdit> {
    let Some(comment_range) = enclosing_node_range(text, byte, &["line_comment"]) else {
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
        "gcl-lint-off" | "gcl-lint-on" | "gcl-lint-off-next" | "gcl-lint-off-file"
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

fn enclosing_node_range(text: &str, byte: usize, kinds: &[&str]) -> Option<Range<usize>> {
    let tree = greycat_analyzer_syntax::parse(text);
    let root = tree.root_node();
    let mut node: Node<'_> = root.descendant_for_byte_range(byte, byte)?;
    loop {
        if kinds.contains(&node.kind()) {
            return Some(node.byte_range());
        }
        node = node.parent()?;
    }
}

/// Scan the body of the function enclosing the param at `param_start`
/// for any text-level occurrence of `name` (whole-word). Returns true
/// iff there are *no* such occurrences (i.e. the rename is safe).
fn is_param_name_unused_in_enclosing_fn(text: &str, param_start: usize, name: &str) -> bool {
    let tree = greycat_analyzer_syntax::parse(text);
    let root = tree.root_node();
    let Some(mut node) = root.descendant_for_byte_range(param_start, param_start) else {
        return true;
    };
    // Walk up to the enclosing function-shaped node.
    loop {
        match node.kind() {
            "fn_decl" | "type_method" | "lambda_expr" => break,
            _ => {}
        }
        let Some(p) = node.parent() else {
            return true;
        };
        node = p;
    }
    let Some(body) = node.child_by_field_name("body") else {
        return true;
    };
    let body_text = &text[body.byte_range()];
    !contains_whole_word(body_text, name)
}

/// Whole-word `name` search in `haystack`. "Whole word" = preceded and
/// followed by a non-`[A-Za-z0-9_]` character (or text boundary).
fn contains_whole_word(haystack: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let nbytes = name.as_bytes();
    let mut i = 0;
    while i + nbytes.len() <= bytes.len() {
        if &bytes[i..i + nbytes.len()] == nbytes {
            let pre_ok =
                i == 0 || !matches!(bytes[i - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
            let post_idx = i + nbytes.len();
            let post_ok = post_idx == bytes.len()
                || !matches!(bytes[post_idx], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_');
            if pre_ok && post_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(code: &str, text: &str, range: Range<usize>) -> Vec<TextEdit> {
        edit_for_diagnostic(text, code, &range, "")
    }

    #[test]
    fn unused_local_removes_whole_var_stmt() {
        // `var foo = bar();` — the lint flags `foo`'s ident range
        // (bytes 14..17). Fix should expand to the full statement.
        let src = "fn f() {\n    var foo = bar();\n    return 0;\n}\n";
        let foo_start = src.find("foo").unwrap();
        let edits = fix("unused-local", src, foo_start..(foo_start + 3));
        assert_eq!(edits.len(), 1);
        // The edit should cover from `var ` through `;` inclusive —
        // i.e. delete the whole `var foo = bar();` slice.
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
    fn unused_param_skipped_when_body_uses_it() {
        // Body references `from` even though the lint may have
        // wrongly flagged it. The fix must refuse so the rename
        // doesn't break the body's reference.
        let src = "fn f(from: time) { var x = from; }\n";
        let p_start = src.find("from:").unwrap();
        let edits = fix("unused-param", src, p_start..(p_start + 4));
        assert!(edits.is_empty(), "expected no edit; got {edits:?}");
    }

    #[test]
    fn unused_param_renames_when_body_doesnt_use_it() {
        let src = "fn f(unused: int) { var x = 0; }\n";
        let p_start = src.find("unused").unwrap();
        let edits = fix("unused-param", src, p_start..(p_start + 6));
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "_unused");
    }

    #[test]
    fn missing_token_inserts_quoted_token() {
        let edits = edit_for_diagnostic("ab", "missing-token", &(2..2), "missing `;`");
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
        // plus its leading space, leaving `// gcl-lint-off-next unused-local`.
        let src =
            "fn main() {\n    // gcl-lint-off-next unused-local unused-param\n    var x = 42;\n}\n";
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
            after.contains("// gcl-lint-off-next unused-local\n"),
            "after = {after:?}"
        );
        assert!(!after.contains("unused-param"), "after = {after:?}");
    }

    #[test]
    fn unused_suppression_drops_first_rule_eats_trailing_space() {
        let src =
            "fn main() {\n    // gcl-lint-off-next unused-local unused-param\n    var y = 0;\n}\n";
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
            after.contains("// gcl-lint-off-next unused-param\n"),
            "after = {after:?}"
        );
    }

    #[test]
    fn unused_suppression_on_sole_rule_deletes_whole_comment_line() {
        let src = "fn main() {\n    // gcl-lint-off-next unused-param\n    var y = 0;\n}\n";
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
            !after.contains("gcl-lint-off-next"),
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
}
