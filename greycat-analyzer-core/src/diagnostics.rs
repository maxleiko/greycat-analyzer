//! Parse-time diagnostic extraction (P1.4).
//!
//! Walks a tree-sitter [`Tree`] and emits one [`Diagnostic`] per `ERROR`
//! or `MISSING` node. The TS reference produces semantically richer parse
//! diagnostics from its hand-rolled CST (it knows what tokens it expected
//! vs. saw); tree-sitter's recovery is more opaque, so we lean on
//! `node.kind()` plus the node's source-text snippet for context.
//!
//! Semantic diagnostics (resolver, type-check, etc.) are out of scope here
//! — they arrive in P2. P15.5 added [`pragma_diagnostics`] so unresolved
//! / duplicate `@include` / `@library` pragmas surface like other diags.

use std::collections::HashSet;
use std::path::Path;

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

use greycat_analyzer_syntax::tree_sitter;

use crate::module_desc::ModuleDesc;
use crate::resolver::{Context, global_std_dir, library_dir};

/// Source string used as the `source` field of every diagnostic this
/// module produces. Lets editors filter / group them.
pub const DIAGNOSTIC_SOURCE: &str = "greycat-analyzer";

/// Walk `root` and return one diagnostic per `ERROR` or `MISSING` node.
/// `source` is the document text — used to render a 1-line snippet of
/// the offending range in the diagnostic message when the node is
/// non-empty.
pub fn parse_diagnostics(root: tree_sitter::Node<'_>, source: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if !root.has_error() && !root.is_missing() {
        return out;
    }
    walk(root, source, &mut out);
    out
}

fn walk(node: tree_sitter::Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    if node.is_missing() {
        out.push(missing_diagnostic(node));
        return;
    }
    if node.is_error() {
        out.push(error_diagnostic(node, source));
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, out);
    }
}

fn error_diagnostic(node: tree_sitter::Node<'_>, source: &str) -> Diagnostic {
    let snippet = source
        .get(node.byte_range())
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let message = if snippet.is_empty() {
        "syntax error".to_string()
    } else {
        // Trim long snippets so we don't blow up tooltip rendering.
        let mut snippet = snippet;
        if snippet.len() > 80 {
            snippet.truncate(77);
            snippet.push_str("...");
        }
        format!("syntax error near `{snippet}`")
    };
    Diagnostic {
        range: node_range(node),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String("parse-error".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn missing_diagnostic(node: tree_sitter::Node<'_>) -> Diagnostic {
    let kind = node.kind();
    let message = if kind.is_empty() {
        "missing token".to_string()
    } else {
        format!("missing `{kind}`")
    };
    Diagnostic {
        range: node_range(node),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String("missing-token".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn node_range(node: tree_sitter::Node<'_>) -> Range {
    let s = node.start_position();
    let e = node.end_position();
    Range {
        start: Position {
            line: s.row as u32,
            character: s.column as u32,
        },
        end: Position {
            line: e.row as u32,
            character: e.column as u32,
        },
    }
}

/// P15.5 — pragma resolution diagnostics. Walks a parsed module's
/// [`ModuleDesc`] and emits warnings for:
///
/// * `unresolved-include` — `@include("path")` whose directory does not
///   exist under `project_dir`.
/// * `unresolved-library` — `@library("name", ...)` not found at
///   `<project_dir>/lib/<name>` (and not under `<greycat_home>/lib/std/`
///   for the global `std` fallback).
/// * `duplicate-include` / `duplicate-library` — second-and-later
///   occurrences of the same pragma path / name in this module.
///
/// `text` is the module's source so byte ranges can be converted to LSP
/// `Position`s. `project_dir` is the entrypoint's parent (where `lib/`
/// and `@include` paths anchor). Pure — no I/O beyond what `ctx.is_dir`
/// performs.
pub fn pragma_diagnostics(
    text: &str,
    desc: &ModuleDesc,
    project_dir: &Path,
    ctx: &dyn Context,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut seen_includes: HashSet<&str> = HashSet::new();
    for inc in &desc.includes {
        if !seen_includes.insert(inc.value.as_str()) {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "duplicate-include",
                DiagnosticSeverity::WARNING,
                format!("duplicate @include('{}')", inc.value),
            ));
            continue;
        }
        let dir = project_dir.join(&inc.value);
        if !ctx.is_dir(&dir) {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "unresolved-include",
                DiagnosticSeverity::WARNING,
                format!("@include('{}'): directory not found", inc.value),
            ));
        }
    }
    let mut seen_libs: HashSet<&str> = HashSet::new();
    for lib in &desc.libraries {
        if !seen_libs.insert(lib.name.as_str()) {
            out.push(make_pragma_diag(
                text,
                &lib.byte_range,
                "duplicate-library",
                DiagnosticSeverity::WARNING,
                format!("duplicate @library('{}')", lib.name),
            ));
            continue;
        }
        let local = library_dir(project_dir, &lib.name);
        let resolved = ctx.is_dir(&local)
            || (lib.name == "std" && ctx.is_dir(&global_std_dir(ctx.greycat_home())));
        if !resolved {
            out.push(make_pragma_diag(
                text,
                &lib.byte_range,
                "unresolved-library",
                DiagnosticSeverity::WARNING,
                format!("@library('{}'): library not found", lib.name),
            ));
        }
    }
    out
}

fn make_pragma_diag(
    text: &str,
    byte_range: &std::ops::Range<usize>,
    code: &str,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: byte_range_to_lsp(text, byte_range),
        severity: Some(severity),
        code: Some(NumberOrString::String(code.to_string())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        ..Default::default()
    }
}

fn byte_range_to_lsp(text: &str, range: &std::ops::Range<usize>) -> Range {
    Range {
        start: position_at(text, range.start),
        end: position_at(text, range.end),
    }
}

fn position_at(text: &str, byte: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let prefix = &text[..byte.min(text.len())];
    for c in prefix.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf8() as u32;
        }
    }
    Position {
        line,
        character: col,
    }
}

/// Format a single diagnostic into the `path:line:col [severity] message`
/// shape the cli lint subcommand prints. The `_` prefix on `code` is a
/// reminder that the rich struct fields (related info, code, tags) get
/// dropped for cli output.
pub fn format_cli(path: &str, diag: &Diagnostic) -> String {
    let severity = match diag.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::HINT) => "hint",
        _ => "diag",
    };
    format!(
        "{}:{}:{}: {}: {}",
        path,
        diag.range.start.line + 1,
        diag.range.start.character + 1,
        severity,
        diag.message,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::module_desc::parse_module_desc;
    use lsp_types::Uri;

    use super::*;

    fn diags(source: &str) -> Vec<Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        parse_diagnostics(tree.root_node(), source)
    }

    /// In-memory `Context` for pragma_diagnostics tests — only `is_dir`
    /// is exercised, the rest are stubs.
    struct PragmaCtx {
        dirs: std::collections::HashSet<PathBuf>,
        greycat_home: PathBuf,
    }

    impl Context for PragmaCtx {
        fn read(&self, _path: &Path) -> std::io::Result<String> {
            Err(std::io::Error::other("stub"))
        }
        fn iter_gcl(&self, _dir: &Path) -> Vec<PathBuf> {
            Vec::new()
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.contains(path)
        }
        fn greycat_home(&self) -> &Path {
            &self.greycat_home
        }
    }

    fn pragma_diags(source: &str, dirs: &[&str]) -> HashMap<String, Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        let uri = Uri::from_str("file:///proj/project.gcl").unwrap();
        let desc = parse_module_desc(uri, source, tree.root_node());
        let ctx = PragmaCtx {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            greycat_home: PathBuf::from("/gcat"),
        };
        let project_dir = Path::new("/proj");
        pragma_diagnostics(source, &desc, project_dir, &ctx)
            .into_iter()
            .map(|d| {
                let code = match &d.code {
                    Some(NumberOrString::String(s)) => s.clone(),
                    _ => String::new(),
                };
                (code, d)
            })
            .collect()
    }
    use std::str::FromStr;

    #[test]
    fn clean_source_produces_no_diagnostics() {
        assert!(diags("fn main() {}\n").is_empty());
    }

    #[test]
    fn missing_token_surfaces() {
        // Trigger an actual missing-token recovery — ERROR-recovery
        // inserts `}` here.
        let ds = diags("fn main() {\n");
        assert!(
            ds.iter()
                .any(|d| d.message.starts_with("missing `}`") || d.message.starts_with("missing")),
            "expected a missing-token diagnostic, got: {ds:?}"
        );
        assert!(
            ds.iter()
                .all(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
        );
        assert!(
            ds.iter()
                .all(|d| d.source.as_deref() == Some(DIAGNOSTIC_SOURCE))
        );
    }

    #[test]
    fn syntax_error_surfaces_with_snippet() {
        // Open paren never closed — produces an ERROR node.
        let ds = diags("fn main( {\n");
        assert!(!ds.is_empty(), "expected at least one diagnostic");
        assert!(ds.iter().any(|d| d.message.starts_with("syntax error")));
    }

    /// P15.5 — `@include` to a missing directory surfaces as
    /// `unresolved-include` with the pragma's range.
    #[test]
    fn pragma_diagnostics_unresolved_include() {
        let src = "@include(\"does_not_exist\");\n";
        let map = pragma_diags(src, &[]);
        assert!(
            map.contains_key("unresolved-include"),
            "expected unresolved-include, got: {map:?}"
        );
        assert_eq!(
            map["unresolved-include"].severity,
            Some(DiagnosticSeverity::WARNING)
        );
    }

    /// P15.5 — duplicate `@include` of the same dir warns on the
    /// second occurrence.
    #[test]
    fn pragma_diagnostics_duplicate_include() {
        let src = "@include(\"a\");\n@include(\"a\");\n";
        let map = pragma_diags(src, &["/proj/a"]);
        assert!(
            map.contains_key("duplicate-include"),
            "expected duplicate-include, got: {map:?}"
        );
        // Second `@include` is line 1 (0-indexed).
        assert_eq!(map["duplicate-include"].range.start.line, 1);
    }

    /// P15.5 — `@library` whose name has no local `lib/` dir and isn't
    /// the global `std` fallback surfaces as `unresolved-library`.
    #[test]
    fn pragma_diagnostics_unresolved_library() {
        let src = "@library(\"missing\", \"1.0\");\n";
        let map = pragma_diags(src, &[]);
        assert!(
            map.contains_key("unresolved-library"),
            "expected unresolved-library, got: {map:?}"
        );
    }

    /// P15.5 — `@library("std", "...")` resolves under the global
    /// `<greycat_home>/lib/std/` fallback when no local `lib/std`
    /// exists; no diagnostic emitted.
    #[test]
    fn pragma_diagnostics_std_resolves_under_greycat_home() {
        let src = "@library(\"std\", \"1.0\");\n";
        let map = pragma_diags(src, &["/gcat/lib/std"]);
        assert!(
            !map.contains_key("unresolved-library"),
            "std should resolve via greycat_home, got: {map:?}"
        );
    }

    /// P15.5 — duplicate `@library` of the same name warns on the
    /// second occurrence.
    #[test]
    fn pragma_diagnostics_duplicate_library() {
        let src = "@library(\"std\", \"1.0\");\n@library(\"std\", \"1.0\");\n";
        let map = pragma_diags(src, &["/gcat/lib/std"]);
        assert!(
            map.contains_key("duplicate-library"),
            "expected duplicate-library, got: {map:?}"
        );
        assert_eq!(map["duplicate-library"].range.start.line, 1);
    }

    #[test]
    fn format_cli_one_indexed() {
        let diag = Diagnostic {
            range: Range {
                start: Position {
                    line: 4,
                    character: 7,
                },
                end: Position {
                    line: 4,
                    character: 9,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: "boom".into(),
            ..Default::default()
        };
        assert_eq!(format_cli("a.gcl", &diag), "a.gcl:5:8: error: boom");
    }
}
