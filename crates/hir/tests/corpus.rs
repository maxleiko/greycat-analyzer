//! Lower the entire vendored corpus through HIR. The assertion is weak
//! intentionally — the goal is "no panics, no infinite loops, a Module
//! falls out the other end" rather than per-file shape checks. Stronger
//! per-shape testing lives in unit tests against curated snippets.
//!
//! Catches regressions in the lowering walker against real-world syntax
//! we ship, including the `inline_type` known-grammar-gap file (which
//! parses with one MISSING `;` but should still lower cleanly modulo
//! that one missing terminator).

use std::path::{Path, PathBuf};

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_syntax::parse;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("hir crate lives under crates/")
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

#[test]
fn lowers_corpus_without_panicking() {
    let root = workspace_root().join("tests/corpus");
    let mut files = Vec::new();
    collect_gcl(&root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "corpus missing under {}", root.display());

    let mut total_decls = 0usize;
    for path in &files {
        let source = std::fs::read_to_string(path).expect("read corpus file");
        let tree = parse(&source);
        let symbols = SymbolTable::default();
        let hir = lower_module(&source, &symbols, "fixture", "project", tree.root_node());
        let module = hir
            .module
            .as_ref()
            .unwrap_or_else(|| panic!("no module produced for {}", path.display()));
        total_decls += module.decls.len();
    }
    eprintln!(
        "[hir:corpus] {} files lowered, {} decls in total",
        files.len(),
        total_decls
    );
}
