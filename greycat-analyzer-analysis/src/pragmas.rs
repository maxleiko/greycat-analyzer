//! Project-pragma lint / fmt control.
//!
//! Walks a module's `mod_pragma > annotation` chain and recognizes
//! `@lint_off("…", "…")` / `@lint_on("…", "…")` pragmas. Returns the
//! two sets (off / on) without judging scope — callers decide whether
//! to merge them into [`ProjectAnalysis`]'s project-wide policy (when
//! the module is the project entrypoint) or store them per-module.
//!
//! Validation diagnostics (`unknown-suppression-rule`,
//! `empty-suppression`, future `conflicting-lint-pragma`) land in P40.3;
//! this walker is intentionally minimal.

use greycat_analyzer_syntax::tree_sitter::Node;
use rustc_hash::FxHashSet;

/// Rule names declared in `@lint_off("…")` / `@lint_on("…")` pragmas at
/// module head. Either set may be empty.
#[derive(Debug, Default, Clone)]
pub struct LintPragmas {
    /// Names from `@lint_off("rule", "rule", …)` annotations.
    pub off: FxHashSet<String>,
    /// Names from `@lint_on("rule", "rule", …)` annotations.
    pub on: FxHashSet<String>,
}

/// Walk every top-level `mod_pragma` in `root` and collect rule names
/// from `@lint_off(...)` / `@lint_on(...)` annotations. Multiple
/// pragmas of the same kind union into one set. Conflicts (a rule
/// named in both `@lint_off` and `@lint_on`) are *not* resolved here —
/// the caller's precedence stack picks the winner.
pub fn parse_lint_pragmas(source: &str, root: Node<'_>) -> LintPragmas {
    let mut out = LintPragmas::default();
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
            let bucket = match name {
                "lint_off" => &mut out.off,
                "lint_on" => &mut out.on,
                _ => continue,
            };
            for rule in string_args(source, args) {
                bucket.insert(rule);
            }
        }
    }
    out
}

/// Yield the raw text of every `string` child of `args`. Mirrors
/// `greycat-analyzer-core`'s `module_desc::string_args` shape but lives
/// in this crate so we don't add a dep arrow.
fn string_args<'src, 'tree>(
    source: &'src str,
    args: Node<'tree>,
) -> impl Iterator<Item = String> + use<'src, 'tree> {
    let mut cursor = args.walk();
    let children: Vec<_> = args
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "string")
        .collect();
    children.into_iter().map(move |s| {
        let mut acc = String::new();
        let mut sc = s.walk();
        for piece in s.named_children(&mut sc) {
            if piece.kind() == "string_fragment" {
                acc.push_str(&source[piece.byte_range()]);
            }
        }
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> LintPragmas {
        let tree = greycat_analyzer_syntax::parse(src);
        parse_lint_pragmas(src, tree.root_node())
    }

    #[test]
    fn lint_off_single_rule() {
        let p = parse("@lint_off(\"unused-decl\");\n");
        assert!(p.off.contains("unused-decl"));
        assert!(p.on.is_empty());
    }

    #[test]
    fn lint_on_single_rule() {
        let p = parse("@lint_on(\"no-breakpoint\");\n");
        assert!(p.on.contains("no-breakpoint"));
        assert!(p.off.is_empty());
    }

    #[test]
    fn multiple_pragmas_union() {
        let src = "@lint_off(\"a\", \"b\");\n@lint_off(\"c\");\n@lint_on(\"d\");\n";
        let p = parse(src);
        assert_eq!(p.off.len(), 3);
        assert!(p.off.contains("a") && p.off.contains("b") && p.off.contains("c"));
        assert_eq!(p.on.len(), 1);
        assert!(p.on.contains("d"));
    }

    #[test]
    fn unrelated_pragmas_ignored() {
        let p = parse("@library(\"std\", \"8.0\");\n@fmt_indent(2);\n");
        assert!(p.off.is_empty());
        assert!(p.on.is_empty());
    }

    #[test]
    fn empty_args_yields_empty_set() {
        let p = parse("@lint_off();\n");
        assert!(p.off.is_empty());
        assert!(p.on.is_empty());
    }
}
