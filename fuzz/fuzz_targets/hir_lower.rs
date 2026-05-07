//! Fuzz target: parse → HIR lower shouldn't panic on arbitrary UTF-8
//! input. Exercises both [`greycat_analyzer_syntax::parse`] and
//! [`greycat_analyzer_hir::lower_module`] together so any panic in
//! the lowering visitor surfaces.
//!
//! Run with: `cargo fuzz run hir_lower`

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let tree = greycat_analyzer_syntax::parse(s);
    let _hir = greycat_analyzer_hir::lower_module(s, "fuzz", "p", tree.root_node());
});
