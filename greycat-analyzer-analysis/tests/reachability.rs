//! End-to-end anchor for the P24 reachability + dead-code feature.
//!
//! Inline unit tests cover every shape in detail; this file ties the
//! whole feature together with the canonical user example so a single
//! `cargo test --test reachability` confirms the lint detects, tags,
//! and fixes correctly.

use greycat_analyzer_analysis::ide::quickfix;
use greycat_analyzer_analysis::lint::{DiagTag, LintSeverity};
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use std::str::FromStr;

const USER_EXAMPLE: &str = "\
enum Option {
    Some,
    None,
}

fn test(x: Option) {
    if (x == Option::Some) {
        return;
    } else if (x == Option::None) {
        return;
    } else {

    }
    var _ = 42;
}
";

fn analyze(src: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///mod.gcl").unwrap();
    mgr.add_simple(uri.clone(), src, "project", false);
    (uri, ProjectAnalysis::analyze(&mgr))
}

#[test]
fn user_example_flags_dead_else_and_post_chain() {
    let (uri, pa) = analyze(USER_EXAMPLE);
    let m = pa.module(&uri).unwrap();
    let unreachable: Vec<_> = m.lints.iter().filter(|l| l.rule == "unreachable").collect();
    assert_eq!(
        unreachable.len(),
        2,
        "expected dead-else + post-chain diagnostics, got {:?}",
        m.lints
    );
    // Both diagnostics should be Hint severity (greyed-out is the
    // editor signal, not warning / error).
    for d in &unreachable {
        assert_eq!(d.severity, LintSeverity::Hint);
    }
    // Both should carry the UNNECESSARY tag so editors dim the span.
    for d in &unreachable {
        assert_eq!(d.tag, Some(DiagTag::Unnecessary));
    }
}

#[test]
fn user_example_quickfix_removes_both_dead_islands() {
    let (uri, pa) = analyze(USER_EXAMPLE);
    let m = pa.module(&uri).unwrap();
    let mut text = USER_EXAMPLE.to_string();

    // Sort dead diagnostics by start descending so deletions don't
    // disturb downstream byte ranges (mirrors `cli lint --fix`'s
    // edit-application order).
    let mut diags: Vec<_> = m.lints.iter().filter(|l| l.rule == "unreachable").collect();
    diags.sort_by_key(|b| std::cmp::Reverse(b.byte_range.start));

    for d in &diags {
        let tree = greycat_analyzer_syntax::parse(&text);
        let edits = quickfix::edit_for_diagnostic(
            tree.root_node(),
            &text,
            d.rule,
            &d.byte_range,
            &d.message,
        );
        assert_eq!(edits.len(), 1, "unreachable should yield exactly one edit");
        text.replace_range(edits[0].byte_range.clone(), &edits[0].new_text);
    }

    // Re-parse must succeed.
    let tree = greycat_analyzer_syntax::parse(&text);
    assert!(
        !tree.root_node().has_error(),
        "fixed source must re-parse cleanly:\n{text}"
    );

    // No dead else, no post-chain `var _`.
    assert!(
        !text.contains("} else {"),
        "expected dead else removed, got:\n{text}"
    );
    assert!(
        !text.contains("var _ = 42"),
        "expected post-chain `var _ = 42` removed, got:\n{text}"
    );

    // Re-running the analyzer on the fixed source should produce
    // zero `unreachable` diagnostics — the fix is convergent in one
    // pass (no fixed-point loop needed).
    let (uri2, pa2) = analyze(&text);
    let m2 = pa2.module(&uri2).unwrap();
    assert!(
        !m2.lints.iter().any(|l| l.rule == "unreachable"),
        "post-fix analysis should have no unreachable left: {:?}",
        m2.lints
    );
}

// P37.3 — `breakpoint;` pauses the worker but execution resumes from the
// next statement after the debugger detaches. The reachability pass must
// NOT classify it alongside the control-flow terminators (return / throw
// / break / continue), or every post-breakpoint statement would dim as
// dead code.
#[test]
fn breakpoint_does_not_terminate_control_flow() {
    let src = "\
fn f(): int {
    breakpoint;
    return 0;
}
";
    let (uri, pa) = analyze(src);
    let m = pa.module(&uri).unwrap();
    let unreachable: Vec<_> = m.lints.iter().filter(|l| l.rule == "unreachable").collect();
    assert!(
        unreachable.is_empty(),
        "`breakpoint;` is not a terminator — `return 0;` after it must stay reachable, got: {:?}",
        unreachable
    );
}
