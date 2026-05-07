//! Coverage gauntlet (P0.5).
//!
//! Bulk-parses two corpora and asserts the resulting tree-sitter trees
//! contain zero `ERROR`/`MISSING` nodes:
//!
//! 1. **Vendored fixtures** — `tests/corpus/{parser,project}_fixtures/`. These
//!    are mirrored from the upstream TS reference repo
//!    (`packages/lang/src/{parser,project}/fixtures/`) and are committed to
//!    the repo. They are an analyzer-port artifact, not a runtime dep.
//!
//! 2. **Stdlib** — `lib/std/*.gcl`. Not vendored. Populated by running
//!    `greycat install` against the repo's `project.gcl` (which pins
//!    `@library("std", "<version>")`). When `lib/std/` is missing, that
//!    block is skipped with a notice rather than failing — so contributors
//!    without GreyCat installed can still iterate on the analyzer.
//!
//! When this test fails, the panic lists every offending file with the
//! `(line, column)` of each ERROR/MISSING node — file grammar gaps upstream
//! against `tree-sitter-greycat` (Decision A / Open Question Q4).

use std::path::{Path, PathBuf};

use greycat_analyzer_syntax::tree_sitter;

/// Files in the corpus where tree-sitter-greycat currently disagrees with the
/// TS reference. Each entry is a workspace-relative path. The gauntlet skips
/// these files entirely. Drop entries from this list as the upstream grammar
/// is fixed.
///
/// Open Question Q4 (ROADMAP §4) — "fix upstream in tree-sitter-greycat or
/// work around in the syntax wrapper? Decide per-gap during P0.5". For each
/// entry below, the resolution is "fix upstream".
const KNOWN_GRAMMAR_GAPS: &[&str] = &[];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("syntax crate has a parent dir")
        .to_path_buf()
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

#[derive(Debug)]
struct Failure {
    path: PathBuf,
    kind: &'static str,
    row: usize,
    column: usize,
    sexp: String,
}

fn find_bad(node: tree_sitter::Node<'_>, path: &Path, out: &mut Vec<Failure>) {
    if !node.has_error() && !node.is_missing() {
        return;
    }
    if node.is_error() || node.is_missing() {
        let pos = node.start_position();
        out.push(Failure {
            path: path.to_path_buf(),
            kind: if node.is_missing() {
                "MISSING"
            } else {
                "ERROR"
            },
            row: pos.row + 1,
            column: pos.column + 1,
            sexp: node.to_sexp(),
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_bad(child, path, out);
    }
}

fn check_corpus(
    label: &str,
    root: &Path,
    workspace: &Path,
    accumulator: &mut Vec<Failure>,
) -> (usize, usize) {
    let mut files = Vec::new();
    collect_gcl(root, &mut files);
    files.sort();
    if files.is_empty() {
        return (0, 0);
    }

    let count = files.len();
    let mut skipped = 0usize;
    let mut total_nodes = 0usize;
    let mut local_failures = 0usize;
    for path in &files {
        let rel = path.strip_prefix(workspace).unwrap_or(path);
        let rel_str = rel.to_str().unwrap_or_default();
        if KNOWN_GRAMMAR_GAPS.contains(&rel_str) {
            skipped += 1;
            continue;
        }
        let source = std::fs::read_to_string(path).expect("read corpus file");
        let tree = greycat_analyzer_syntax::parse(&source);
        total_nodes += tree.root_node().descendant_count();
        let before = accumulator.len();
        find_bad(tree.root_node(), path, accumulator);
        local_failures += accumulator.len() - before;
    }
    eprintln!(
        "[corpus:{label}] {} of {count} files, {total_nodes} nodes, {local_failures} bad sub-trees ({skipped} skipped via KNOWN_GRAMMAR_GAPS)",
        count - skipped,
    );
    (count, skipped)
}

#[test]
fn parses_corpus_without_errors() {
    let root = workspace_root();
    let vendored = root.join("tests/corpus");
    let stdlib = root.join("lib/std");

    let mut failures: Vec<Failure> = Vec::new();
    let (vendored_count, _) = check_corpus("vendored", &vendored, &root, &mut failures);
    assert!(
        vendored_count > 0,
        "no .gcl files under {}; corpus is missing",
        vendored.display()
    );

    if stdlib.is_dir() {
        check_corpus("stdlib", &stdlib, &root, &mut failures);
    } else {
        eprintln!(
            "[corpus:stdlib] {} not present — run `greycat install` from repo root \
             to enable stdlib coverage. Skipping.",
            stdlib.display()
        );
    }

    if !failures.is_empty() {
        let mut msg = format!(
            "tree-sitter-greycat reported {} ERROR/MISSING node(s):\n",
            failures.len()
        );
        for f in &failures {
            let rel = f.path.strip_prefix(&root).unwrap_or(&f.path);
            msg.push_str(&format!(
                "  {}:{}:{}  [{}]  {}\n",
                rel.display(),
                f.row,
                f.column,
                f.kind,
                f.sexp,
            ));
        }
        panic!("{msg}");
    }
}
