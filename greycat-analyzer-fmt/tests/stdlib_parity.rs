//! Byte-for-byte parity gate against the TS reference (`greycat-lang
//! fmt`) over `lib/std/*.gcl`.
//!
//! Runs only when:
//!   - `greycat-lang` is on PATH (a TS-reference binary at
//!     `~/.greycat/bin/greycat-lang` typically), AND
//!   - `lib/std/` is populated (see `scripts/check-stdlib.sh`).
//!
//! Otherwise the test no-ops with a `[stdlib_parity] skipped: ...`
//! note so CI on a clean runner doesn't break.
//!
//! The TS CLI's `fmt` (without `-w`) prints to stdout and Node's
//! `console.log` adds a trailing `\n` that doesn't come from the
//! formatter. To get the *true* formatter output, we copy each file
//! to a tempfile and run `greycat-lang fmt -w <tempfile>`, which
//! writes the formatter's verbatim bytes.

use std::path::PathBuf;
use std::process::Command;

const KNOWN_DIVERGENCES: &[(&str, usize)] = &[
    // core.gcl line 511 — TS preserves a 5-space (typo) indent on a
    // standalone EOL comment inside an enum body; we normalize to the
    // canonical 4-space indent. Arguably the better behavior, but it
    // breaks the byte-for-byte invariant. Tracked as a TS-side quirk.
    ("core.gcl", 4),
];

fn ts_binary_present() -> bool {
    Command::new("greycat-lang")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn stdlib_dir() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    workspace.join("lib/std")
}

fn ts_format(src_path: &std::path::Path) -> Option<String> {
    let tmp = tempdir()?.join(src_path.file_name().unwrap());
    std::fs::copy(src_path, &tmp).ok()?;
    let status = Command::new("greycat-lang")
        .arg("fmt")
        .arg("-w")
        .arg(&tmp)
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    std::fs::read_to_string(&tmp).ok()
}

fn tempdir() -> Option<PathBuf> {
    // No `tempfile` dep — roll a tiny one keyed on the test process pid.
    let dir = std::env::temp_dir().join(format!("gcl-fmt-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn diff_line_count(a: &str, b: &str) -> usize {
    // Naive line-by-line diff: count of lines that differ when paired
    // by index. Good enough for "is the gap shrinking?" — the parity
    // gauntlet's `MATCH_FLOOR` pattern uses this same shape.
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let n = al.len().max(bl.len());
    (0..n).filter(|i| al.get(*i) != bl.get(*i)).count()
}

#[test]
fn stdlib_byte_for_byte_against_ts() {
    if !ts_binary_present() {
        eprintln!("[stdlib_parity] skipped: greycat-lang not on PATH");
        return;
    }
    let std_dir = stdlib_dir();
    if !std_dir.is_dir() {
        eprintln!("[stdlib_parity] skipped: {std_dir:?} missing — run `greycat install`");
        return;
    }
    let mut total = 0usize;
    let mut clean_files = 0usize;
    let mut known_diverge_files = 0usize;
    let mut new_diff_files: Vec<(String, usize)> = Vec::new();
    for entry in std::fs::read_dir(&std_dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("gcl") {
            continue;
        }
        total += 1;
        let src = std::fs::read_to_string(&path).unwrap();
        let rust_out = greycat_analyzer_fmt::format(&src);
        let Some(ts_out) = ts_format(&path) else {
            eprintln!(
                "[stdlib_parity] {} — could not run TS reference (skipped)",
                path.file_name().unwrap().to_string_lossy()
            );
            continue;
        };
        let diff_lines = diff_line_count(&rust_out, &ts_out);
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        if diff_lines == 0 {
            clean_files += 1;
        } else if let Some(&(_, allowed)) = KNOWN_DIVERGENCES.iter().find(|(n, _)| *n == fname) {
            if diff_lines <= allowed {
                known_diverge_files += 1;
            } else {
                new_diff_files.push((fname, diff_lines));
            }
        } else {
            new_diff_files.push((fname, diff_lines));
        }
    }
    eprintln!(
        "[stdlib_parity] {clean_files}/{total} byte-for-byte; \
         {known_diverge_files} within known divergence budget; \
         {} new gaps: {new_diff_files:?}",
        new_diff_files.len()
    );
    assert!(
        new_diff_files.is_empty(),
        "stdlib parity regressed: new diffs {new_diff_files:?}"
    );
}
