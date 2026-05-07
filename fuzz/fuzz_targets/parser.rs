//! Fuzz target: tree-sitter-greycat shouldn't panic on arbitrary
//! UTF-8 input. Exercises [`greycat_analyzer_syntax::parse`].
//!
//! Run with: `cargo fuzz run parser`

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _tree = greycat_analyzer_syntax::parse(s);
});
