// P1.4 — parse-time diagnostic extraction. P15.5 added
// [`pragma_diagnostics`].
//! Parse-time diagnostic extraction.
//!
//! Walks a tree-sitter [`Tree`] and emits one [`Diagnostic`] per `ERROR`
//! or `MISSING` node. The TS reference produces semantically richer parse
//! diagnostics from its hand-rolled CST (it knows what tokens it expected
//! vs. saw); tree-sitter's recovery is more opaque, so we lean on
//! `node.kind()` plus the node's source-text snippet for context.
//!
//! Semantic diagnostics (resolver, type-check, etc.) are out of scope here
//! — they arrive separately. [`pragma_diagnostics`] surfaces unresolved /
//! duplicate `@include` / `@library` pragmas like other diags.

use std::path::Path;

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use rustc_hash::FxHashSet;

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

// P15.5
/// Pragma resolution diagnostics. Walks a parsed module's
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
    let mut seen_includes: FxHashSet<&str> = FxHashSet::default();
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
        // P15.x — runtime rejects absolute paths in @include (it
        // joins the path as `./<value>`), so flag them and don't run
        // the dir-existence check.
        if Path::new(&inc.value).is_absolute() {
            out.push(make_pragma_diag(
                text,
                &inc.byte_range,
                "absolute-include",
                DiagnosticSeverity::WARNING,
                format!(
                    "@include('{}'): absolute paths are not supported (use a project-relative path)",
                    inc.value
                ),
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
    let mut seen_libs: FxHashSet<&str> = FxHashSet::default();
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
        // P17.4 — a library is "resolved" when at least one of the
        // following holds:
        //   1. `<project>/lib/<name>/` is a directory (the canonical
        //      code-library shape).
        //   2. `<project>/webroot/<name>/` is a directory (asset-only
        //      libraries like `explorer` ship as webroot bundles with
        //      no `.gcl` content).
        //   3. `<project>/lib/installed` lists `<name>=...` (the manifest
        //      `greycat install` writes; counts even when the dir hasn't
        //      been materialized in this checkout).
        //   4. The `std` fallback at `<greycat_home>/lib/std/`.
        // Only when *none* of these match do we surface a diagnostic.
        let local = library_dir(project_dir, &lib.name);
        let webroot = project_dir.join("webroot").join(&lib.name);
        let resolved = ctx.is_dir(&local)
            || ctx.is_dir(&webroot)
            || installed_manifest_lists(ctx, project_dir, &lib.name)
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

/// `true` iff `<project>/lib/installed` exists and contains a line
/// starting with `<name>=`. The `installed` manifest is what
/// `greycat install` writes when it materializes a library, and a
/// name being listed there is a strong signal the library is meant
/// to be present even if its directory hasn't been extracted yet.
fn installed_manifest_lists(ctx: &dyn Context, project_dir: &std::path::Path, name: &str) -> bool {
    let manifest = project_dir.join("lib").join("installed");
    let Ok(text) = ctx.read(&manifest) else {
        return false;
    };
    let prefix = format!("{name}=");
    text.lines().any(|line| line.starts_with(&prefix))
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

// P32.5
/// File-spanning advisory: this `.gcl` file is not part of any
/// GreyCat project (no `project.gcl` was found walking up from its
/// directory to its workspace folder root). Used by the LSP server
/// alongside parse diagnostics to dim the whole file in the editor
/// and explain why nothing else is being analysed.
///
/// Tagged `UNNECESSARY` so VSCode / other editors render the file
/// greyed out. Severity is `Information` — this is guidance, not
/// an error.
pub fn orphan_module_diagnostic(text: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: position_at(text, text.len()),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: Some(NumberOrString::String("orphan-module".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "This file is not part of any GreyCat project (no `project.gcl` was found from this file's directory up to the workspace folder root). Add a `project.gcl` to enable full analysis.".into(),
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

// P33.1
/// File-spanning error: the GreyCat `std` library couldn't be found
/// (neither `<project_dir>/lib/std/` nor `<greycat_home>/lib/std/`).
/// The analyzer can't run real type-checking without std, so this
/// dims the project.gcl and explains why every other module is
/// drowning in "unresolved type" diagnostics.
///
/// Severity is `Error` (this is a hard blocker for any meaningful
/// analysis) and the diag is also tagged `UNNECESSARY` so editors
/// dim the whole file as a visual cue.
pub fn missing_std_diagnostic(text: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: position_at(text, text.len()),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("missing-std".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message: "GreyCat `std` library not found. Looked under `<project>/lib/std/` and `$HOME/.greycat/lib/std/`. Run `greycat install` (or populate the local `lib/std/`) — without std the analyzer can't resolve built-in types.".into(),
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
    }
}

// P32.6
/// File-spanning advisory: this `.gcl` file is reachable from
/// multiple projects' `@include` closures. Lists the conflicting
/// project roots so the user can collapse the overlap if it's
/// unintended.
///
/// Tagged `UNNECESSARY` (dim) and `Information` severity.
pub fn multi_project_owner_diagnostic(text: &str, roots: &[std::path::PathBuf]) -> Diagnostic {
    let mut roots_msg = String::new();
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            roots_msg.push_str(", ");
        }
        roots_msg.push_str(&r.display().to_string());
    }
    let message = format!(
        "This file is reachable from multiple GreyCat projects ({roots_msg}). Analysis is anchored to the first owner; if the overlap is unintended, restructure your `@include` paths so only one project includes it."
    );
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: position_at(text, text.len()),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: Some(NumberOrString::String("multi-project-owner".into())),
        source: Some(DIAGNOSTIC_SOURCE.into()),
        message,
        tags: Some(vec![lsp_types::DiagnosticTag::UNNECESSARY]),
        ..Default::default()
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
    use std::path::PathBuf;

    use rustc_hash::FxHashMap;

    use crate::module_desc::parse_module_desc;
    use lsp_types::Uri;

    use super::*;

    fn diags(source: &str) -> Vec<Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        parse_diagnostics(tree.root_node(), source)
    }

    // P17.4 added the optional `path → contents` map.
    /// In-memory `Context` for pragma_diagnostics tests. Tracks
    /// known directories plus an optional `path → contents` map for
    /// reading the `lib/installed` manifest.
    struct PragmaCtx {
        dirs: FxHashSet<PathBuf>,
        files: FxHashMap<PathBuf, String>,
        greycat_home: PathBuf,
    }

    impl Context for PragmaCtx {
        fn read(&self, path: &Path) -> std::io::Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::other("not found"))
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

    fn pragma_diags(source: &str, dirs: &[&str]) -> FxHashMap<String, Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        let uri = Uri::from_str("file:///proj/project.gcl").unwrap();
        let desc = parse_module_desc(uri, source, tree.root_node());
        let ctx = PragmaCtx {
            dirs: dirs.iter().map(PathBuf::from).collect(),
            files: FxHashMap::default(),
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

    // P15.5
    /// `@include` to a missing directory surfaces as
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

    // P15.x
    /// Runtime rejects absolute paths in `@include`; the
    /// analyzer mirrors that with an `absolute-include` warning.
    #[test]
    fn pragma_diagnostics_absolute_include() {
        let src = "@include(\"/tmp/anything\");\n";
        let map = pragma_diags(src, &["/tmp/anything"]);
        assert!(
            map.contains_key("absolute-include"),
            "expected absolute-include warning, got: {map:?}"
        );
        // The dir-not-found check should be skipped — emitting both
        // would be noisy.
        assert!(
            !map.contains_key("unresolved-include"),
            "absolute path should not also fire unresolved-include: {map:?}"
        );
    }

    // P15.5
    /// Duplicate `@include` of the same dir warns on the
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

    // P15.5
    /// `@library` whose name has no local `lib/` dir and isn't
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

    // P17.4
    /// `@library("explorer", ...)` resolves to a webroot
    /// asset library: `<project>/webroot/<name>/` exists even though
    /// `<project>/lib/<name>/` does not.
    #[test]
    fn pragma_diagnostics_library_resolves_from_webroot() {
        let src = "@library(\"explorer\", \"1.0\");\n";
        let map = pragma_diags(src, &["/proj/webroot/explorer"]);
        assert!(
            !map.contains_key("unresolved-library"),
            "expected webroot fallback to resolve, got: {map:?}"
        );
    }

    // P17.4
    /// `@library("foo", ...)` resolves when `lib/installed`
    /// lists the name. Useful for asset-only libs that haven't yet
    /// extracted their dir.
    #[test]
    fn pragma_diagnostics_library_resolves_from_installed_manifest() {
        let src = "@library(\"foo\", \"1.0\");\n";
        let tree = greycat_analyzer_syntax::parse(src);
        let uri = Uri::from_str("file:///proj/project.gcl").unwrap();
        let desc = parse_module_desc(uri, src, tree.root_node());
        let mut files = FxHashMap::default();
        files.insert(
            PathBuf::from("/proj/lib/installed"),
            "std=8.0.269-dev\nfoo=1.0\n".into(),
        );
        let ctx = PragmaCtx {
            dirs: Default::default(),
            files,
            greycat_home: PathBuf::from("/gcat"),
        };
        let project_dir = Path::new("/proj");
        let out = pragma_diagnostics(src, &desc, project_dir, &ctx);
        assert!(
            !out.iter().any(
                |d| matches!(&d.code, Some(NumberOrString::String(s)) if s == "unresolved-library")
            ),
            "expected `lib/installed` manifest to resolve `foo`, got: {out:?}"
        );
    }

    // P15.5
    /// `@library("std", "...")` resolves under the global
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

    // P15.5
    /// Duplicate `@library` of the same name warns on the
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
