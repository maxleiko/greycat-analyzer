//! Snapshot harness (P0.6).
//!
//! For every `.gcl` file under `tests/corpus/`, parses it and snapshots an
//! indented s-expression of the resulting tree-sitter CST. The snapshots
//! live under `greycat-analyzer-syntax/snapshots/` and are reviewed via
//! `cargo insta review`.
//!
//! This is the **Rust-side** half of the parity oracle described in
//! ROADMAP §7 (test strategy A). The TS-reference half — diff Rust port
//! output against the TS port over the same corpus — comes online at the
//! layers where both sides produce comparable artifacts: diagnostics JSON
//! (P1.4) and formatter output (P4.1). Tree-sitter's CST has no equivalent
//! in the TS reference (which uses a hand-rolled `cst.Node` representation),
//! so a raw-CST cross-port diff is intentionally out of scope.
//!
//! Why bother with snapshots if there's nothing to diff against on the TS
//! side yet? Because they catch *Rust-side* regressions: grammar bumps,
//! accidental `tree.edit()` glitches in `Document`, refactors that change
//! whitespace handling. New corpus drift surfaces as a review prompt
//! instead of a silent change.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use greycat_analyzer_syntax::tree_sitter;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("syntax crate lives under crates/")
        .to_path_buf()
}

fn corpus_root() -> PathBuf {
    workspace_root().join("tests/corpus")
}

fn collect_gcl(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_gcl(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("gcl") {
            out.push(path);
        }
    }
}

/// Render a tree-sitter node as an indented s-expression. Named children
/// are emitted with their field name (when present), one per line. Anonymous
/// nodes are skipped — they're punctuation/keywords and add noise.
///
/// Leaf named nodes (`ident`, `number`, `string_fragment`, etc.) emit their
/// source text in single quotes; non-leaf named nodes emit the kind only.
fn sexp(node: tree_sitter::Node<'_>, source: &str, indent: usize, field: Option<&str>) -> String {
    let mut out = String::new();
    write_sexp(&mut out, node, source, indent, field);
    out
}

fn write_sexp(
    out: &mut String,
    node: tree_sitter::Node<'_>,
    source: &str,
    indent: usize,
    field: Option<&str>,
) {
    let pad = "  ".repeat(indent);
    let field_prefix = field.map(|f| format!("{f}: ")).unwrap_or_default();
    let kind = node.kind();

    let mut cursor = node.walk();
    let has_named_children = node.named_children(&mut cursor).next().is_some();

    if !has_named_children {
        let text = node.utf8_text(source.as_bytes()).unwrap_or("");
        let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
        if escaped.is_empty() {
            let _ = writeln!(out, "{pad}{field_prefix}({kind})");
        } else {
            let _ = writeln!(out, "{pad}{field_prefix}({kind} '{escaped}')");
        }
        return;
    }

    let _ = writeln!(out, "{pad}{field_prefix}({kind}");

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().is_named() {
                let child_field = cursor.field_name();
                write_sexp(out, cursor.node(), source, indent + 1, child_field);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let _ = writeln!(out, "{pad})");
}

#[test]
fn snapshot_corpus() {
    let root = corpus_root();
    let mut files = Vec::new();
    collect_gcl(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "no .gcl files under {}; corpus is missing",
        root.display()
    );

    insta::with_settings!({
        snapshot_path => "../snapshots",
        prepend_module_to_snapshot => false,
    }, {
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read corpus file");
            let tree = greycat_analyzer_syntax::parse(&source);
            let rendered = sexp(tree.root_node(), &source, 0, None);
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .with_extension("");
            let snapshot_name = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "__");
            insta::assert_snapshot!(snapshot_name, rendered, &source);
        }
    });
}
