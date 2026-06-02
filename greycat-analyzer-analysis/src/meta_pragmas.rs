//! Project-pragma lint / fmt control.
//!
//! Walks a module's `mod_pragma > annotation` chain and recognizes
//! `@lint_off("…", "…")` / `@lint_on("…", "…")` pragmas. Returns the
//! two sets (off / on) without judging scope — callers decide whether
//! to merge them into [`ProjectAnalysis`]'s project-wide policy (when
//! the module is the project entrypoint) or store them per-module.
//!
//! Validation diagnostics (`unknown-suppression-rule`,
//! `empty-suppression`, `conflicting-lint-pragma`) ride alongside the
//! recognized rule sets in [`LintPragmas::diagnostics`].

use greycat_analyzer_syntax::tree_sitter::Node;
use rustc_hash::FxHashSet;

use crate::lint::{DiagTag, LintDiagnostic, LintSeverity};

/// Rule names declared in `@lint_off("…")` / `@lint_on("…")` pragmas at
/// module head, plus any validation diagnostics produced while parsing
/// them (`unknown-suppression-rule`, `empty-suppression`,
/// `conflicting-lint-pragma`).
#[derive(Debug, Default, Clone)]
pub struct LintPragmas {
    /// Names from `@lint_off("rule", "rule", …)` annotations.
    pub off: FxHashSet<String>,
    /// Names from `@lint_on("rule", "rule", …)` annotations.
    pub on: FxHashSet<String>,
    /// Diagnostics emitted by the walker itself. Seeded into
    /// `module.lints` so CLI / LSP surface them alongside regular
    /// lints — same flow as `Directives::diagnostics`.
    pub diagnostics: Vec<LintDiagnostic>,
}

/// Walk every top-level `mod_pragma` in `root` and collect rule names
/// from `@lint_off(...)` / `@lint_on(...)` annotations.
///
/// `is_entrypoint` controls scope: project-wide lint policy lives only
/// in `project.gcl`. When `false`, every `@lint_off` / `@lint_on` is
/// rejected as `lint-pragma-outside-entrypoint` (Warning) with a
/// quickfix that deletes the offending pragma, and the rules are
/// **not** added to `off` / `on` (so the caller never applies module-
/// scope pragmas). When `true`, multiple pragmas of the same kind
/// union; the walker also emits:
///
/// - empty argument list (`@lint_off();`) → `empty-suppression`.
/// - unknown rule name → `unknown-suppression-rule` on the literal.
/// - rule named in both `@lint_off` and `@lint_on` in the same
///   entrypoint → `conflicting-lint-pragma` on the offending pragma
///   (Warning, no auto-fix — the user has to decide).
pub fn parse_lint_pragmas(source: &str, root: Node<'_>, is_entrypoint: bool) -> LintPragmas {
    let mut out = LintPragmas::default();
    let valid_rules: FxHashSet<&'static str> =
        crate::lint::LINT_RULES.iter().map(|r| r.name).collect();
    let mut walker = root.walk();
    for child in root.named_children(&mut walker) {
        if child.kind() != "mod_pragma" {
            continue;
        }
        // Remember the pragma's full span — used for the entrypoint-only
        // quickfix when `is_entrypoint == false`.
        let pragma_span = child.byte_range();
        let mut sub = child.walk();
        for c in child.named_children(&mut sub) {
            if c.kind() != "annotation" {
                continue;
            }
            let mut ann = c.walk();
            let mut name: Option<&str> = None;
            let mut args: Option<Node<'_>> = None;
            for ac in c.named_children(&mut ann) {
                match ac.kind() {
                    "ident" => name = Some(&source[ac.byte_range()]),
                    "args" => args = Some(ac),
                    _ => {}
                }
            }
            let (Some(name), Some(args)) = (name, args) else {
                continue;
            };
            let is_off = match name {
                "lint_off" => true,
                "lint_on" => false,
                _ => continue,
            };
            // P40.5 — pragma found in a non-entrypoint module. Reject:
            // emit the new diagnostic, discard the rules, skip the
            // empty/unknown/conflict validation (those would just stack
            // on top and confuse the editor).
            if !is_entrypoint {
                out.diagnostics.push(LintDiagnostic {
                    rule: "lint-pragma-outside-entrypoint",
                    severity: LintSeverity::Warning,
                    message: format!(
                        "`@{name}` may only appear in the project's entrypoint (`project.gcl`) so \
                         lint policy lives in one place — move this pragma there (or delete it)"
                    ),
                    byte_range: pragma_span.clone(),
                    tag: None,
                });
                continue;
            }
            // Collect (rule_name, source_range_of_arg) so we can flag
            // unknown names with precise spans.
            let mut harvested: Vec<(String, std::ops::Range<usize>)> = Vec::new();
            for (rule, range) in string_args_with_ranges(source, args) {
                harvested.push((rule, range));
            }
            if harvested.is_empty() {
                // Empty argument list — same diagnostic shape the line
                // directive parser uses for `// gcl-lint-off` (no rule).
                out.diagnostics.push(LintDiagnostic {
                    rule: "empty-suppression",
                    severity: LintSeverity::Warning,
                    message: format!("`@{name}` requires at least one rule name (no wildcard)"),
                    byte_range: c.byte_range(),
                    tag: None,
                });
                continue;
            }
            for (rule_name, span) in harvested {
                if !valid_rules.contains(rule_name.as_str()) {
                    out.diagnostics.push(LintDiagnostic {
                        rule: "unknown-suppression-rule",
                        severity: LintSeverity::Warning,
                        message: format!("unknown lint rule `{rule_name}` in `@{name}`"),
                        byte_range: span,
                        tag: None,
                    });
                    continue;
                }
                if is_off {
                    if out.on.contains(rule_name.as_str()) {
                        out.diagnostics.push(LintDiagnostic {
                            rule: "conflicting-lint-pragma",
                            severity: LintSeverity::Warning,
                            message: format!(
                                "`{rule_name}` is named in both `@lint_off` and `@lint_on` in this \
                                 module — `@lint_off` wins; remove one of the two"
                            ),
                            byte_range: span.clone(),
                            tag: None,
                        });
                    }
                    out.off.insert(rule_name);
                } else {
                    if out.off.contains(rule_name.as_str()) {
                        out.diagnostics.push(LintDiagnostic {
                            rule: "conflicting-lint-pragma",
                            severity: LintSeverity::Warning,
                            message: format!(
                                "`{rule_name}` is named in both `@lint_off` and `@lint_on` in this \
                                 module — `@lint_off` wins; remove one of the two"
                            ),
                            byte_range: span.clone(),
                            tag: None,
                        });
                    }
                    out.on.insert(rule_name);
                }
            }
        }
    }
    // Tag-fill: editors render UNNECESSARY-tagged diagnostics dimmed,
    // and `conflicting-lint-pragma` qualifies (the conflicting line is
    // editorially "dead").
    for diag in &mut out.diagnostics {
        if diag.tag.is_none() {
            diag.tag = match diag.rule {
                "conflicting-lint-pragma" => Some(DiagTag::Unnecessary),
                _ => None,
            };
        }
    }
    out
}

/// Yield `(content, span)` for every `string` child of `args`. The
/// content is the concatenated text of `string_fragment` children; the
/// span is the whole `string` node's byte range (including the quotes)
/// so validation diagnostics can point at the literal cleanly.
fn string_args_with_ranges<'src, 'tree>(
    source: &'src str,
    args: Node<'tree>,
) -> impl Iterator<Item = (String, std::ops::Range<usize>)> + use<'src, 'tree> {
    let mut cursor = args.walk();
    let children: Vec<_> = args
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "string")
        .collect();
    children.into_iter().map(move |s| {
        let span = s.byte_range();
        let mut acc = String::new();
        let mut sc = s.walk();
        for piece in s.named_children(&mut sc) {
            if piece.kind() == "string_fragment" {
                acc.push_str(&source[piece.byte_range()]);
            }
        }
        (acc, span)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> LintPragmas {
        let tree = greycat_analyzer_syntax::parse(src);
        parse_lint_pragmas(src, tree.root_node(), true)
    }

    fn parse_module(src: &str) -> LintPragmas {
        let tree = greycat_analyzer_syntax::parse(src);
        parse_lint_pragmas(src, tree.root_node(), false)
    }

    #[test]
    fn lint_off_single_rule() {
        let p = parse("@lint_off(\"unused-decl\");\n");
        assert!(p.off.contains("unused-decl"));
        assert!(p.on.is_empty());
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn lint_on_single_rule() {
        let p = parse("@lint_on(\"no-breakpoint\");\n");
        assert!(p.on.contains("no-breakpoint"));
        assert!(p.off.is_empty());
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn multiple_pragmas_union() {
        let src = "@lint_off(\"unused-decl\", \"unused-local\");\n@lint_off(\"unused-param\");\n\
             @lint_on(\"no-breakpoint\");\n";
        let p = parse(src);
        assert_eq!(p.off.len(), 3);
        assert!(
            p.off.contains("unused-decl")
                && p.off.contains("unused-local")
                && p.off.contains("unused-param")
        );
        assert_eq!(p.on.len(), 1);
        assert!(p.on.contains("no-breakpoint"));
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn unrelated_pragmas_ignored() {
        let p = parse("@library(\"std\", \"8.0\");\n@fmt_indent(2);\n");
        assert!(p.off.is_empty());
        assert!(p.on.is_empty());
        assert!(p.diagnostics.is_empty());
    }

    #[test]
    fn unknown_rule_name_emits_diagnostic() {
        let p = parse("@lint_off(\"bogus-rule\");\n");
        assert!(p.off.is_empty(), "unknown rule must not enter the set");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "unknown-suppression-rule");
        assert!(p.diagnostics[0].message.contains("bogus-rule"));
    }

    #[test]
    fn empty_args_emits_empty_suppression() {
        let p = parse("@lint_off();\n");
        assert!(p.off.is_empty());
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "empty-suppression");
    }

    #[test]
    fn off_then_on_same_rule_emits_conflict_on_the_on_pragma() {
        let p = parse("@lint_off(\"unused-decl\");\n@lint_on(\"unused-decl\");\n");
        assert!(p.off.contains("unused-decl"));
        assert!(p.on.contains("unused-decl"));
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "conflicting-lint-pragma");
    }

    #[test]
    fn on_then_off_same_rule_also_emits_conflict() {
        let p = parse("@lint_on(\"unused-decl\");\n@lint_off(\"unused-decl\");\n");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "conflicting-lint-pragma");
    }

    // P40.5 — in non-entrypoint modules, pragmas are rejected with
    // `lint-pragma-outside-entrypoint` and do NOT populate the off / on
    // sets. Empty / unknown / conflict validation is suppressed
    // (those would just stack on top).

    #[test]
    fn module_pragma_rejected_with_lint_pragma_outside_entrypoint() {
        let p = parse_module("@lint_off(\"unused-decl\");\n");
        assert!(p.off.is_empty(), "module-scope pragma must not apply");
        assert!(p.on.is_empty());
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "lint-pragma-outside-entrypoint");
    }

    #[test]
    fn module_pragma_skips_empty_and_unknown_validation() {
        // The user moved a stale `@lint_off()` from project.gcl. The
        // walker should NOT emit empty-suppression on top of the
        // entrypoint-only warning — only one diagnostic surfaces.
        let p = parse_module("@lint_off();\n");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].rule, "lint-pragma-outside-entrypoint");
    }
}
