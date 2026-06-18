//! Per-fixture parity gauntlet against `tests/corpus/parser_fixtures/`.
//!
//! After P21.5 the gauntlet is **hard equality**: every `in.gcl` must
//! format byte-for-byte to its sibling `out.gcl`, and every `out.gcl`
//! must be a fixed point of `format` (idempotency). The earlier
//! `MATCH_FLOOR` / `IDEMPOTENT_FLOOR` regression budgets are gone —
//! they were ratchets while the new pipeline was being built; the
//! pipeline now meets the M5 / M9 acceptance criteria.

use std::path::PathBuf;

#[test]
fn formatter_parity_against_corpus() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
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
    assert_eq!(matches, total, "formatter parity broke on: {mismatches:?}");
}

#[test]
fn formatter_idempotent_on_corpus() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
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
    assert_eq!(
        idempotent, total,
        "formatter idempotency broke on: {violators:?}"
    );
}
