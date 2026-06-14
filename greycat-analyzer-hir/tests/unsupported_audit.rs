//! Audit: which CST kinds still land as `Expr::Unsupported`?
//!
//! Walks `lib/std/*.gcl` (when present) plus the parser fixtures and
//! prints a histogram. Skipped (passes vacuously) when stdlib isn't
//! installed — keeps CI green on hosts without GreyCat.

use std::collections::BTreeMap;
use std::path::PathBuf;

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::hir::Expr;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_syntax::parse;

#[test]
fn enumerate_unsupported_kinds() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let stdlib = workspace.join("lib/std");
    let fixtures = workspace.join("tests/corpus/parser_fixtures");

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut visited = 0usize;
    for root in [&stdlib, &fixtures] {
        if !root.is_dir() {
            continue;
        }
        collect(root, &mut counts, &mut visited);
    }

    if visited == 0 {
        eprintln!("[unsupported_audit] no .gcl files found — skipping");
        return;
    }

    let mut sorted: Vec<_> = counts.iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(**v));
    eprintln!(
        "[unsupported_audit] {} files, {} distinct Unsupported kinds:",
        visited,
        sorted.len()
    );
    for (k, v) in &sorted {
        eprintln!("  {v:5}  {k}");
    }
    // P7.2 acceptance: zero `Expr::Unsupported` over stdlib + corpus.
    // The audit is a regression guard — if a future grammar / lowering
    // change re-introduces unsupported shapes, this test is what flags
    // them. Allow up to the audit's pre-acceptance baseline (0).
    assert!(
        sorted.is_empty(),
        "Expr::Unsupported regressed — kinds: {sorted:?}"
    );
}

fn collect(dir: &std::path::Path, counts: &mut BTreeMap<String, usize>, visited: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, counts, visited);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("gcl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        *visited += 1;
        let tree = parse(&text);
        let symbols = SymbolTable::default();
        let hir = lower_module(&text, &symbols, "m", "p", tree.root_node());
        for (_, e) in hir.exprs.iter() {
            if let Expr::Unsupported { kind, .. } = e {
                *counts.entry((*kind).to_string()).or_default() += 1;
            }
        }
    }
}
