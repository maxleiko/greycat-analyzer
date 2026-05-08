//! Per-fixture parity gauntlet (P9.2).
//!
//! For every `tests/corpus/parser_fixtures/<n>/{in.gcl,out.gcl}` pair,
//! format `in.gcl` and compare against `out.gcl`. The test reports a
//! per-fixture match / mismatch and asserts that the total *match
//! count* doesn't decrease — a regression budget that lets P9.1's
//! honest-first-pass progress show up in CI as the formatter improves.
//!
//! When formatter parity becomes complete (M9 acceptance), this test
//! flips to require all fixtures pass.

use std::path::PathBuf;

#[test]
fn formatter_parity_against_corpus() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let fixtures = workspace.join("tests/corpus/parser_fixtures");
    if !fixtures.is_dir() {
        eprintln!("[parity_gauntlet] no fixtures dir — skipping");
        return;
    }
    let mut total = 0usize;
    let mut matches = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&fixtures).unwrap().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let in_path = path.join("in.gcl");
        let out_path = path.join("out.gcl");
        if !in_path.is_file() || !out_path.is_file() {
            continue;
        }
        let input = std::fs::read_to_string(&in_path).unwrap();
        let expected = std::fs::read_to_string(&out_path).unwrap();
        let formatted = greycat_analyzer_fmt::format(&input);
        total += 1;
        if formatted == expected {
            matches += 1;
        } else {
            mismatches.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    eprintln!(
        "[parity_gauntlet] {matches}/{total} fixtures format byte-for-byte; mismatches: {mismatches:?}"
    );
    // Regression budget: at least the fixtures that match today must
    // continue to match. Bump as P14.3 lands more rules. The two
    // remaining hard cases need line-length-aware reflow (args_split,
    // nested_args_split, if_var_object) plus the comment + annotation
    // ordering edge case (doc_eol_stmt, stmts_rules).
    const MATCH_FLOOR: usize = 3;
    let _ = total;
    assert!(
        matches >= MATCH_FLOOR,
        "formatter parity regressed: {matches} matches < floor {MATCH_FLOOR}; mismatches: {mismatches:?}"
    );
}

#[test]
fn formatter_idempotent_on_corpus() {
    // Honest first-pass status — the property `fmt(fmt(x)) == fmt(x)`
    // doesn't hold on every corpus fixture today (string-literal
    // whitespace handling has a known bug). Until P9.1 is complete
    // we report mismatches without failing the build.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let fixtures = workspace.join("tests/corpus/parser_fixtures");
    if !fixtures.is_dir() {
        return;
    }
    let mut total = 0usize;
    let mut idempotent = 0usize;
    let mut violators: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&fixtures).unwrap().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let out_path = path.join("out.gcl");
        if !out_path.is_file() {
            continue;
        }
        let original = std::fs::read_to_string(&out_path).unwrap();
        let once = greycat_analyzer_fmt::format(&original);
        let twice = greycat_analyzer_fmt::format(&once);
        total += 1;
        if once == twice {
            idempotent += 1;
        } else {
            violators.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    eprintln!("[idempotency] {idempotent}/{total} fixtures idempotent; violators: {violators:?}");
    // Regression budget: prevent slip below today's baseline. Bump as
    // P14.3 / P9.1 lands fixes.
    const IDEMPOTENT_FLOOR: usize = 6;
    let _ = total;
    assert!(
        idempotent >= IDEMPOTENT_FLOOR,
        "formatter idempotency regressed: {idempotent} idempotent < floor {IDEMPOTENT_FLOOR}; violators: {violators:?}"
    );
}
