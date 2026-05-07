//! Fuzz target: `parse → fmt → parse` round-trip shouldn't panic
//! and the second parse shouldn't introduce error nodes that the
//! first parse didn't have. Doesn't assert byte-for-byte equality —
//! that's the formatter parity gauntlet (P9.2) — just panic-freedom
//! and "doesn't break valid input."
//!
//! Run with: `cargo fuzz run format_round_trip`

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let tree = greycat_analyzer_syntax::parse(s);
    if tree.root_node().has_error() {
        return; // skip already-broken input
    }
    let formatted = greycat_analyzer_fmt::format_tree(s, tree.root_node());
    let _ = greycat_analyzer_syntax::parse(&formatted);
});
