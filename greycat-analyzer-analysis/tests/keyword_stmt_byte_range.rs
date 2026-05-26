//! Regression test for byte_range on keyword-only statements
//! (`return ;`, `return expr;`, `throw …;`, `break;`, `continue;`,
//! `breakpoint;`).
//!
//! Before the fix, `Stmt::Return(Option<Idx<Expr>>)` and the other
//! keyword-only variants carried no span. `stmt_byte_range` fell back
//! to `0..0` for bare returns, which made the dead-code lint anchor
//! its "unreachable code" diagnostic at position 1:1 (module start)
//! whenever the dead suffix's first stmt was a bare `return ;` — for
//! example, a trailing return after an exhaustive enum-eq chain.
//!
//! The fix gives every keyword-only Stmt variant a `byte_range` taken
//! from the CST node, so the diagnostic now points at the keyword.

use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

fn unreachable_ranges(pa: &ProjectAnalysis, uri: &Uri) -> Vec<std::ops::Range<usize>> {
    let m = pa.module(uri).expect("module");
    m.lints
        .iter()
        .filter(|d| d.rule == "unreachable")
        .map(|d| d.byte_range.clone())
        .collect()
}

#[test]
fn unreachable_bare_return_after_exhaustive_chain_locates_keyword() {
    // The reported FP: trailing `return ;` after an exhaustive
    // enum-eq chain. The range must cover the bare return, not
    // collapse to `0..0`.
    let src = "\
enum Color { Red; Green; Blue; }
fn pick(c: Color): float {
    if (c == Color::Red) {
        return 1.0;
    } else if (c == Color::Green) {
        return 2.0;
    } else if (c == Color::Blue) {
        return 3.0;
    }
    return ;
}
";
    let (uri, pa) = analyze(src);
    let ranges = unreachable_ranges(&pa, &uri);
    assert_eq!(
        ranges.len(),
        1,
        "expected exactly one unreachable diagnostic, got: {ranges:?}",
    );
    let r = &ranges[0];
    assert!(
        r.start > 0,
        "expected the range to start past the module header, got {r:?}",
    );
    let snippet = &src[r.clone()];
    assert!(
        snippet.contains("return"),
        "expected the range to cover the `return` keyword, got snippet {snippet:?} (range {r:?})",
    );
}

#[test]
fn unreachable_bare_break_in_loop_locates_keyword() {
    // `while (true) { return; break; }` — break is unreachable.
    let src = "\
fn loopy() {
    while (true) {
        return;
        break;
    }
}
";
    let (uri, pa) = analyze(src);
    let ranges = unreachable_ranges(&pa, &uri);
    assert_eq!(
        ranges.len(),
        1,
        "expected exactly one unreachable diagnostic, got: {ranges:?}",
    );
    let r = &ranges[0];
    assert!(
        r.start > 0,
        "expected the range to start past the module header, got {r:?}",
    );
    let snippet = &src[r.clone()];
    assert!(
        snippet.contains("break"),
        "expected the range to cover the `break` keyword, got snippet {snippet:?} (range {r:?})",
    );
}
