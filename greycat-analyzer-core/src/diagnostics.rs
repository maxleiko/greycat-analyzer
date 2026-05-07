//! Parse-time diagnostic extraction (P1.4).
//!
//! Walks a tree-sitter [`Tree`] and emits one [`Diagnostic`] per `ERROR`
//! or `MISSING` node. The TS reference produces semantically richer parse
//! diagnostics from its hand-rolled CST (it knows what tokens it expected
//! vs. saw); tree-sitter's recovery is more opaque, so we lean on
//! `node.kind()` plus the node's source-text snippet for context.
//!
//! Semantic diagnostics (resolver, type-check, etc.) are out of scope here
//! — they arrive in P2.

use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use greycat_analyzer_syntax::tree_sitter;

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
    use super::*;

    fn diags(source: &str) -> Vec<Diagnostic> {
        let tree = greycat_analyzer_syntax::parse(source);
        parse_diagnostics(tree.root_node(), source)
    }

    #[test]
    fn clean_source_produces_no_diagnostics() {
        assert!(diags("fn main() {}\n").is_empty());
    }

    #[test]
    fn missing_token_surfaces() {
        // `inline_type` style — missing trailing `;`. The grammar wants
        // a terminator after each `type_attr`.
        let ds = diags("type Foo { a: int; b: float }\n");
        assert!(
            ds.iter().any(|d| d.message.starts_with("missing `;`")),
            "expected a missing-`;` diagnostic, got: {ds:?}"
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
