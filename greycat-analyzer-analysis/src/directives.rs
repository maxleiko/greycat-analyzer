//! Comment-driven directives for opting out of lints / formatter (P23).
//!
//! Walk the CST for `line_comment` extras whose payload starts with the
//! `gcl-` prefix. Two directive families:
//!
//! - **Lint suppressions** — `gcl-lint-off <rule>...` /
//!   `gcl-lint-on <rule>...` / `gcl-lint-off-next <rule>...` /
//!   `gcl-lint-off-file <rule>...`. Each carries an explicit rule list
//!   (no wildcard). `-off` and `-on` form pairs; `-off-next` covers the
//!   next AST item; `-off-file` covers the whole file (only at module
//!   head).
//! - **Formatter skip** — `gcl-fmt-off` / `gcl-fmt-on` / `gcl-fmt-skip`
//!   / `gcl-fmt-off-file`. `-off`/`-on` form pairs (verbatim region);
//!   `-skip` covers the next AST node only; `-off-file` covers the
//!   whole file (only at module head).
//!
//! Built once per source. Lint emission consults
//! [`Directives::suppresses_lint`] via [`crate::lint::LintCx::emit`];
//! formatter consults [`Directives::fmt_skips`] when lowering nodes.
//!
//! Misspelled rule names emit `unknown-suppression-rule`; empty rule
//! lists on `-off` / `-off-next` / `-off-file` emit `empty-suppression`.

use std::collections::HashSet;
use std::ops::Range;

use greycat_analyzer_syntax::tree_sitter::Node;

use crate::lint::{LINT_RULES, LintDiagnostic, LintSeverity};

/// One rule named in a directive comment, paired with its byte range
/// in the source. The range points at the rule word itself (not the
/// whole comment), so `unused-suppression` can underline the *specific*
/// dead rule rather than the whole comment line — important when a
/// directive lists several rules and only some are dead weight.
#[derive(Debug, Clone)]
pub struct RuleEntry {
    pub name: String,
    /// Absolute source byte range of this rule name. Points into the
    /// directive comment that introduced it.
    pub byte_range: Range<usize>,
}

/// Lint suppression — silences one or more rules over a byte range.
#[derive(Debug, Clone)]
pub struct LintSuppression {
    /// Source byte range whose diagnostics should be silenced.
    pub byte_range: Range<usize>,
    /// Explicitly named rules + their per-rule byte ranges.
    /// Wildcards aren't allowed (P23 spec).
    pub rules: Vec<RuleEntry>,
    /// What kind of directive produced this entry — needed when the
    /// `unused-suppression` rule decides which slot to flag.
    pub kind: LintSuppressionKind,
    /// Source byte range of the directive comment itself. Used as a
    /// fallback for diagnostics that have no specific rule slot to
    /// point at (e.g. `empty-suppression` on a bare `// gcl-lint-off`).
    pub directive_range: Range<usize>,
    /// Per-rule "did this suppression actually drop a diagnostic?"
    /// tracking. Mutated by [`Directives::suppresses_lint`] when a
    /// match occurs. Populated *only* for the rules in `rules` —
    /// never `unused-suppression` itself, which would be circular.
    pub used_rules: HashSet<String>,
}

/// Shape of the originating directive — drives `unused-suppression`'s
/// reporting position when the comment turns out to be dead weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSuppressionKind {
    /// `gcl-lint-off <rules>` paired with `gcl-lint-on <rules>` (or EOF).
    Range,
    /// `gcl-lint-off-next <rules>` covering the next AST item.
    NextItem,
    /// `gcl-lint-off-file <rules>` covering the whole file.
    File,
}

/// Formatter skip — preserves a byte range verbatim through formatting.
#[derive(Debug, Clone)]
pub struct FmtSkipRange {
    /// Source byte range whose contents should be emitted verbatim.
    pub byte_range: Range<usize>,
    /// Shape of the originating directive.
    pub kind: FmtSkipKind,
    /// Source byte range of the directive comment itself.
    pub directive_range: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtSkipKind {
    /// `gcl-fmt-off` paired with `gcl-fmt-on` (or EOF).
    Range,
    /// `gcl-fmt-skip` covering the next AST node.
    NextNode,
    /// `gcl-fmt-off-file` covering the whole file.
    File,
}

/// Result of [`Directives::parse`]. Holds both directive families plus
/// any diagnostics emitted while parsing the directive comments
/// themselves (`unknown-suppression-rule`, `empty-suppression`,
/// `unbalanced-fmt-off`, …).
#[derive(Debug, Default, Clone)]
pub struct Directives {
    pub lint_suppressions: Vec<LintSuppression>,
    pub fmt_skips: Vec<FmtSkipRange>,
    /// Diagnostics emitted by the directive parser. Folded into the
    /// per-module lint stream by callers so they surface alongside
    /// regular lints.
    pub diagnostics: Vec<LintDiagnostic>,
}

impl Directives {
    /// True when `byte` falls inside a suppression covering `rule`.
    /// Records the hit on the matching suppression so
    /// `unused-suppression` can compute deadness later.
    pub fn suppresses_lint(&mut self, byte: usize, rule: &str) -> bool {
        let mut hit = false;
        for s in &mut self.lint_suppressions {
            if s.byte_range.contains(&byte) && s.rules.iter().any(|r| r.name == rule) {
                s.used_rules.insert(rule.to_string());
                hit = true;
            }
        }
        hit
    }

    /// True when `byte_range` falls fully inside a fmt-skip range.
    pub fn fmt_skipped(&self, byte_range: &Range<usize>) -> bool {
        self.fmt_skips
            .iter()
            .any(|s| s.byte_range.start <= byte_range.start && byte_range.end <= s.byte_range.end)
    }

    /// True when the whole file should be emitted verbatim (a
    /// `gcl-fmt-off-file` was seen at module head).
    pub fn fmt_off_file(&self) -> bool {
        self.fmt_skips
            .iter()
            .any(|s| matches!(s.kind, FmtSkipKind::File))
    }
}

#[derive(Debug)]
struct OpenLintOff {
    start: usize,
    rules: Vec<RuleEntry>,
    directive_range: Range<usize>,
}

#[derive(Debug)]
struct OpenFmtOff {
    start: usize,
    directive_range: Range<usize>,
}

#[derive(Debug)]
struct RawComment<'a> {
    /// `// ...` payload exactly as it appears in source (including the
    /// leading `//`).
    text: &'a str,
    byte_range: Range<usize>,
    node: Node<'a>,
}

#[derive(Debug)]
enum Directive {
    LintOff(Vec<RuleEntry>),
    LintOn(Vec<RuleEntry>),
    LintOffNext(Vec<RuleEntry>),
    LintOffFile(Vec<RuleEntry>),
    FmtOff,
    FmtOn,
    FmtSkip,
    FmtOffFile,
}

/// Parse a `// gcl-…` line comment. Tracks per-rule byte positions
/// (within the source) so `unused-suppression` /
/// `unknown-suppression-rule` can underline the *specific* rule word
/// rather than the whole comment line.
///
/// `comment_start` is the absolute source byte offset of `comment_text`
/// (= `raw.byte_range.start`). Returns `None` for non-directive
/// comments.
fn parse_directive(comment_text: &str, comment_start: usize) -> Option<Directive> {
    let bytes = comment_text.as_bytes();
    if bytes.len() < 2 || &bytes[..2] != b"//" {
        return None;
    }
    let mut i = 2;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let head = &comment_text[name_start..i];
    let rest = parse_rule_list(comment_text, i, comment_start);
    let rest_empty = rest.is_empty();
    match head {
        "gcl-lint-off" => Some(Directive::LintOff(rest)),
        "gcl-lint-on" => Some(Directive::LintOn(rest)),
        "gcl-lint-off-next" => Some(Directive::LintOffNext(rest)),
        "gcl-lint-off-file" => Some(Directive::LintOffFile(rest)),
        "gcl-fmt-off" if rest_empty => Some(Directive::FmtOff),
        "gcl-fmt-on" if rest_empty => Some(Directive::FmtOn),
        "gcl-fmt-skip" if rest_empty => Some(Directive::FmtSkip),
        "gcl-fmt-off-file" if rest_empty => Some(Directive::FmtOffFile),
        _ => None,
    }
}

/// Walk the rule-list slice (everything after the directive name) and
/// emit a [`RuleEntry`] per whitespace-delimited word with its absolute
/// source byte range.
///
/// `start` is the offset within `comment_text` to begin scanning;
/// `comment_start` is the absolute source offset of `comment_text`.
fn parse_rule_list(comment_text: &str, start: usize, comment_start: usize) -> Vec<RuleEntry> {
    let mut out = Vec::new();
    let bytes = comment_text.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let word_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if word_start < i {
            out.push(RuleEntry {
                name: comment_text[word_start..i].to_string(),
                byte_range: (comment_start + word_start)..(comment_start + i),
            });
        }
    }
    out
}

fn is_known_lint_rule(name: &str) -> bool {
    LINT_RULES.iter().any(|r| r.name == name)
}

/// Byte range of the next AST item that follows `comment_node`. Walks
/// up the tree until we find a sibling — handles the case where the
/// comment is the last child of its parent (we look at the parent's
/// next sibling instead).
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

/// True when `comment_node` sits at module head — its parent is the
/// `module` root and no real decl precedes it.
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

/// Top-level entry — walks the tree and returns the parsed directives.
/// Source text is required so directive comments can be classified
/// from their byte content.
pub fn parse_directives(source: &str, root: Node<'_>) -> Directives {
    let mut comments = Vec::new();
    {
        let mut cursor = root.walk();
        walk_for_comments(&mut cursor, source, &mut comments);
    }
    parse_with_collected(source, root, comments)
}

fn walk_for_comments<'a>(
    cursor: &mut greycat_analyzer_syntax::tree_sitter::TreeCursor<'a>,
    source: &'a str,
    out: &mut Vec<RawComment<'a>>,
) {
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
            walk_for_comments(cursor, source, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn parse_with_collected(
    source: &str,
    _root: Node<'_>,
    comments: Vec<RawComment<'_>>,
) -> Directives {
    // Re-use Directives::parse's logic by short-circuiting through it
    // — but Directives::parse currently re-walks the tree. Instead, we
    // inline the same logic here so the per-comment text is the slice
    // we already extracted (rather than left empty by the placeholder
    // collector above).
    let source_end = source.len();
    let mut out = Directives::default();
    let mut open_lint: Vec<OpenLintOff> = Vec::new();
    let mut open_fmt: Option<OpenFmtOff> = None;

    for raw in &comments {
        let Some(parsed) = parse_directive(raw.text, raw.byte_range.start) else {
            continue;
        };
        match parsed {
            Directive::LintOff(rules) => {
                if rules.is_empty() {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "empty-suppression",
                        severity: LintSeverity::Warning,
                        message: "`gcl-lint-off` requires at least one rule name (no wildcard)"
                            .into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                for r in &rules {
                    if !is_known_lint_rule(&r.name) {
                        out.diagnostics.push(LintDiagnostic {
                            rule: "unknown-suppression-rule",
                            severity: LintSeverity::Warning,
                            message: format!("unknown lint rule `{}`", r.name),
                            byte_range: r.byte_range.clone(),
                        });
                    }
                }
                open_lint.push(OpenLintOff {
                    start: raw.byte_range.end,
                    rules,
                    directive_range: raw.byte_range.clone(),
                });
            }
            Directive::LintOn(rules) => {
                // Empty `-on` closes everything currently open across
                // all rules; named `-on rule0 rule1` closes those rules
                // only. The `LintSuppression`s emitted on close carry
                // the OFF-side rule entries (so unused-suppression
                // points at the OFF comment, not the ON comment).
                let target_names: Vec<String> = if rules.is_empty() {
                    let mut acc: HashSet<String> = HashSet::new();
                    for o in &open_lint {
                        for r in &o.rules {
                            acc.insert(r.name.clone());
                        }
                    }
                    acc.into_iter().collect()
                } else {
                    rules.iter().map(|r| r.name.clone()).collect()
                };
                for name in &target_names {
                    let mut idx_to_remove: Option<usize> = None;
                    for (idx, o) in open_lint.iter().enumerate().rev() {
                        if o.rules.iter().any(|x| x.name == *name) {
                            idx_to_remove = Some(idx);
                            break;
                        }
                    }
                    if let Some(idx) = idx_to_remove {
                        let slot = &mut open_lint[idx];
                        // Pull the closing rule's entry out of the open
                        // slot so the resulting suppression carries the
                        // OFF-side byte range.
                        let entry_pos = slot.rules.iter().position(|x| x.name == *name);
                        if let Some(pos) = entry_pos {
                            let entry = slot.rules.remove(pos);
                            out.lint_suppressions.push(LintSuppression {
                                byte_range: slot.start..raw.byte_range.start,
                                rules: vec![entry],
                                kind: LintSuppressionKind::Range,
                                directive_range: slot.directive_range.clone(),
                                used_rules: HashSet::new(),
                            });
                        }
                        if slot.rules.is_empty() {
                            open_lint.remove(idx);
                        }
                    }
                }
            }
            Directive::LintOffNext(rules) => {
                if rules.is_empty() {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "empty-suppression",
                        severity: LintSeverity::Warning,
                        message: "`gcl-lint-off-next` requires at least one rule name".into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                for r in &rules {
                    if !is_known_lint_rule(&r.name) {
                        out.diagnostics.push(LintDiagnostic {
                            rule: "unknown-suppression-rule",
                            severity: LintSeverity::Warning,
                            message: format!("unknown lint rule `{}`", r.name),
                            byte_range: r.byte_range.clone(),
                        });
                    }
                }
                let next_range =
                    next_ast_item_range(raw.node).unwrap_or(raw.byte_range.end..raw.byte_range.end);
                out.lint_suppressions.push(LintSuppression {
                    byte_range: next_range,
                    rules,
                    kind: LintSuppressionKind::NextItem,
                    directive_range: raw.byte_range.clone(),
                    used_rules: HashSet::new(),
                });
            }
            Directive::LintOffFile(rules) => {
                if rules.is_empty() {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "empty-suppression",
                        severity: LintSeverity::Warning,
                        message: "`gcl-lint-off-file` requires at least one rule name".into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                if !is_at_module_head(raw.node) {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "empty-suppression",
                        severity: LintSeverity::Warning,
                        message: "`gcl-lint-off-file` must appear before any decl at module head"
                            .into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                for r in &rules {
                    if !is_known_lint_rule(&r.name) {
                        out.diagnostics.push(LintDiagnostic {
                            rule: "unknown-suppression-rule",
                            severity: LintSeverity::Warning,
                            message: format!("unknown lint rule `{}`", r.name),
                            byte_range: r.byte_range.clone(),
                        });
                    }
                }
                out.lint_suppressions.push(LintSuppression {
                    byte_range: 0..source_end,
                    rules,
                    kind: LintSuppressionKind::File,
                    directive_range: raw.byte_range.clone(),
                    used_rules: HashSet::new(),
                });
            }
            Directive::FmtOff => {
                if open_fmt.is_some() {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "unbalanced-fmt-off",
                        severity: LintSeverity::Warning,
                        message: "`gcl-fmt-off` already active — nested toggle ignored".into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                open_fmt = Some(OpenFmtOff {
                    start: raw.byte_range.end,
                    directive_range: raw.byte_range.clone(),
                });
            }
            Directive::FmtOn => {
                if let Some(open) = open_fmt.take() {
                    out.fmt_skips.push(FmtSkipRange {
                        byte_range: open.start..raw.byte_range.start,
                        kind: FmtSkipKind::Range,
                        directive_range: open.directive_range,
                    });
                } else {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "unbalanced-fmt-off",
                        severity: LintSeverity::Warning,
                        message: "`gcl-fmt-on` without matching `gcl-fmt-off`".into(),
                        byte_range: raw.byte_range.clone(),
                    });
                }
            }
            Directive::FmtSkip => {
                let next_range =
                    next_ast_item_range(raw.node).unwrap_or(raw.byte_range.end..raw.byte_range.end);
                out.fmt_skips.push(FmtSkipRange {
                    byte_range: next_range,
                    kind: FmtSkipKind::NextNode,
                    directive_range: raw.byte_range.clone(),
                });
            }
            Directive::FmtOffFile => {
                if !is_at_module_head(raw.node) {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "empty-suppression",
                        severity: LintSeverity::Warning,
                        message: "`gcl-fmt-off-file` must appear before any decl at module head"
                            .into(),
                        byte_range: raw.byte_range.clone(),
                    });
                    continue;
                }
                out.fmt_skips.push(FmtSkipRange {
                    byte_range: 0..source_end,
                    kind: FmtSkipKind::File,
                    directive_range: raw.byte_range.clone(),
                });
            }
        }
    }

    for slot in open_lint {
        out.diagnostics.push(LintDiagnostic {
            rule: "unbalanced-lint-off",
            severity: LintSeverity::Warning,
            message: "`gcl-lint-off` without matching `gcl-lint-on` — extends to EOF".into(),
            byte_range: slot.directive_range.clone(),
        });
        for r in slot.rules {
            out.lint_suppressions.push(LintSuppression {
                byte_range: slot.start..source_end,
                rules: vec![r],
                kind: LintSuppressionKind::Range,
                directive_range: slot.directive_range.clone(),
                used_rules: HashSet::new(),
            });
        }
    }

    if let Some(open) = open_fmt {
        out.diagnostics.push(LintDiagnostic {
            rule: "unbalanced-fmt-off",
            severity: LintSeverity::Warning,
            message: "`gcl-fmt-off` without matching `gcl-fmt-on` — extends to EOF".into(),
            byte_range: open.directive_range.clone(),
        });
        out.fmt_skips.push(FmtSkipRange {
            byte_range: open.start..source_end,
            kind: FmtSkipKind::Range,
            directive_range: open.directive_range,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_syntax::parse;

    fn dirs(src: &str) -> Directives {
        let tree = parse(src);
        parse_directives(src, tree.root_node())
    }

    #[test]
    fn lint_off_next_resolves_to_next_decl() {
        let src = "// gcl-lint-off-next unused-decl\nprivate fn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.lint_suppressions.len(), 1);
        let s = &d.lint_suppressions[0];
        let names: Vec<&str> = s.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["unused-decl"]);
        // Per-rule byte range points at the rule word inside the comment.
        let rule_slice = &src[s.rules[0].byte_range.clone()];
        assert_eq!(rule_slice, "unused-decl");
        assert_eq!(s.kind, LintSuppressionKind::NextItem);
        // `byte_range` covers the `private fn foo() {}` decl text.
        let covered = &src[s.byte_range.clone()];
        assert!(
            covered.contains("private fn foo()"),
            "covered = {covered:?}"
        );
    }

    #[test]
    fn lint_off_on_pair_brackets_range() {
        let src = "// gcl-lint-off unused-decl\nprivate fn foo() {}\n// gcl-lint-on unused-decl\n";
        let d = dirs(src);
        assert_eq!(d.lint_suppressions.len(), 1);
        let s = &d.lint_suppressions[0];
        assert_eq!(s.kind, LintSuppressionKind::Range);
        let names: Vec<&str> = s.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["unused-decl"]);
        // The OFF-side rule range — i.e. the "unused-decl" word in the
        // OFF comment — should be carried into the closed suppression.
        let rule_slice = &src[s.rules[0].byte_range.clone()];
        assert_eq!(rule_slice, "unused-decl");
        assert!(
            s.rules[0].byte_range.start < src.find("\nprivate").unwrap(),
            "OFF-side rule range should sit inside the OFF comment, not the ON",
        );
    }

    #[test]
    fn rule_byte_range_points_at_specific_rule_word() {
        // Multiple rules in one comment: each rule's byte range points
        // at *that* rule word, not the whole comment. Drives
        // unused-suppression's per-rule diagnostic placement.
        let src = "// gcl-lint-off-next unused-local unused-param\nfn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.lint_suppressions.len(), 1);
        let s = &d.lint_suppressions[0];
        assert_eq!(s.rules.len(), 2);
        assert_eq!(&src[s.rules[0].byte_range.clone()], "unused-local");
        assert_eq!(&src[s.rules[1].byte_range.clone()], "unused-param");
    }

    #[test]
    fn empty_rule_list_emits_diagnostic() {
        let src = "// gcl-lint-off\nfn foo() {}\n";
        let d = dirs(src);
        assert!(d.lint_suppressions.is_empty());
        assert!(d.diagnostics.iter().any(|x| x.rule == "empty-suppression"));
    }

    #[test]
    fn unknown_rule_emits_diagnostic() {
        let src = "// gcl-lint-off-next not-a-rule\nfn foo() {}\n";
        let d = dirs(src);
        assert!(
            d.diagnostics
                .iter()
                .any(|x| x.rule == "unknown-suppression-rule")
        );
    }

    #[test]
    fn lint_off_file_must_be_at_module_head() {
        let src = "fn foo() {}\n// gcl-lint-off-file unused-decl\n";
        let d = dirs(src);
        assert!(d.lint_suppressions.is_empty());
        assert!(d.diagnostics.iter().any(|x| x.rule == "empty-suppression"));
    }

    #[test]
    fn lint_off_file_at_module_head_covers_whole_file() {
        let src = "// gcl-lint-off-file unused-decl\nfn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.lint_suppressions.len(), 1);
        let s = &d.lint_suppressions[0];
        assert_eq!(s.kind, LintSuppressionKind::File);
        assert_eq!(s.byte_range, 0..src.len());
    }

    #[test]
    fn fmt_off_on_pair_records_skip_range() {
        let src = "// gcl-fmt-off\nfn foo() {}\n// gcl-fmt-on\n";
        let d = dirs(src);
        assert_eq!(d.fmt_skips.len(), 1);
        assert_eq!(d.fmt_skips[0].kind, FmtSkipKind::Range);
    }

    #[test]
    fn fmt_skip_resolves_to_next_node() {
        let src = "// gcl-fmt-skip\nfn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.fmt_skips.len(), 1);
        assert_eq!(d.fmt_skips[0].kind, FmtSkipKind::NextNode);
    }

    #[test]
    fn unbalanced_fmt_off_extends_to_eof_with_warning() {
        let src = "// gcl-fmt-off\nfn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.fmt_skips.len(), 1);
        assert!(d.diagnostics.iter().any(|x| x.rule == "unbalanced-fmt-off"));
    }

    #[test]
    fn unbalanced_lint_off_extends_to_eof_with_warning() {
        let src = "// gcl-lint-off unused-decl\nprivate fn foo() {}\n";
        let d = dirs(src);
        assert_eq!(d.lint_suppressions.len(), 1);
        assert!(
            d.diagnostics
                .iter()
                .any(|x| x.rule == "unbalanced-lint-off")
        );
    }

    #[test]
    fn suppresses_lint_marks_used_rule() {
        let src = "// gcl-lint-off-next unused-decl\nprivate fn foo() {}\n";
        let mut d = dirs(src);
        let foo_byte = src.find("private").unwrap() + 1;
        assert!(d.suppresses_lint(foo_byte, "unused-decl"));
        assert!(d.lint_suppressions[0].used_rules.contains("unused-decl"));
    }

    #[test]
    fn suppresses_lint_unknown_rule_returns_false() {
        let src = "// gcl-lint-off-next unused-decl\nprivate fn foo() {}\n";
        let mut d = dirs(src);
        let foo_byte = src.find("private").unwrap() + 1;
        assert!(!d.suppresses_lint(foo_byte, "unused-local"));
    }
}
