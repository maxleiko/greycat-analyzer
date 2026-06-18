//! Array-literal element-type inference. The analyzer mimics the
//! GreyCat runtime's "constant-evaluable" trigger rules (verified
//! empirically against `greycat run`) so `var x = [42, 1337]` types
//! as `Array<int>` instead of bare `Array`. The trigger is purely
//! syntactic: idents / calls / member access / etc. disqualify;
//! object_expr trigger doesn't recurse into its slots.
//!
//! The *element type itself* uses the analyzer's full inference, so
//! binary expressions over typed literals (`42time - 10s`) come out
//! correctly as `time` even though the runtime mis-infers them as
//! `duration`. That deliberate deviation is asserted by
//! `binary_time_minus_duration_uses_analyzer_inference`.

use greycat_analyzer_analysis::display::display_type;
use greycat_analyzer_analysis::project::ProjectAnalysis;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::hir::{Decl, Stmt};
use std::str::FromStr;

/// Synthetic `std/core.gcl` with the well-known native types the
/// analyzer dispatches against. Mirrors the in-crate test fixture in
/// `well_known.rs` — needed because `add_simple` doesn't load real
/// stdlib and `well_known.array_decl` must be populated for the
/// `Array<T>` shape to materialize.
const STD_CORE: &str = "\
native type any {}\n\
native type null {}\n\
native type bool {}\n\
native type int {}\n\
native type float {}\n\
native type String {}\n\
native type char {}\n\
native type time {}\n\
native type duration {}\n\
native type Array<T> {}\n\
";

fn analyze(extra: &str) -> (Uri, ProjectAnalysis) {
    let mut mgr = SourceManager::new();
    let std_uri = Uri::from_str("file:///std/core.gcl").unwrap();
    mgr.add_simple(std_uri, STD_CORE, "std", false);
    let user_uri = Uri::from_str("file:///proj/main.gcl").unwrap();
    mgr.add_simple(user_uri.clone(), extra, "project", false);
    (user_uri, ProjectAnalysis::analyze(&mgr))
}

/// Return the inferred type of `var x = <expr>` in the first fn body
/// of `uri`, formatted via `display_type`.
fn x_type(pa: &ProjectAnalysis, uri: &Uri) -> String {
    let m = pa.module(uri).expect("module");
    let arena = pa.arena();
    let decl_registry = pa.decl_registry();
    let symbols = pa.symbols();
    for (_id, decl) in m.hir.decls.iter() {
        let Decl::Fn(fnd) = decl else { continue };
        let Some(body) = fnd.body else { continue };
        let Stmt::Block(block) = &m.hir.stmts[body] else {
            continue;
        };
        for stmt_id in block.stmts.iter() {
            let Stmt::Var(lv) = &m.hir.stmts[*stmt_id] else {
                continue;
            };
            if &symbols[m.hir.idents[lv.name].symbol] != "x" {
                continue;
            }
            let ty = m
                .analysis
                .def_types
                .get(&lv.name)
                .copied()
                .expect("def_type for x");
            return display_type(arena, decl_registry, symbols, ty).to_string();
        }
    }
    panic!("var `x` not found");
}

fn array_type(expr: &str) -> String {
    let src = format!("fn main() {{\n    var x = {expr};\n}}\n");
    let (uri, pa) = analyze(&src);
    x_type(&pa, &uri)
}

fn array_type_with_decls(decls: &str, expr: &str) -> String {
    let src = format!("{decls}\nfn main() {{\n    var x = {expr};\n}}\n");
    let (uri, pa) = analyze(&src);
    x_type(&pa, &uri)
}

// --- positive: shape passes, all elements same type → Array<T> ---

#[test]
fn int_literals_infer_array_int() {
    assert_eq!(array_type("[42, 1337]"), "Array<int>");
}

#[test]
fn float_literals_infer_array_float() {
    assert_eq!(array_type("[3.14, 1.337]"), "Array<float>");
}

#[test]
fn bool_literals_infer_array_bool() {
    assert_eq!(array_type("[true, false]"), "Array<bool>");
}

#[test]
fn string_literals_infer_array_string() {
    assert_eq!(array_type(r#"["foo", "bar"]"#), "Array<String>");
}

#[test]
fn time_literals_infer_array_time() {
    assert_eq!(array_type("[42time, 10time]"), "Array<time>");
}

#[test]
fn duration_literals_infer_array_duration() {
    assert_eq!(array_type("[42_us, 10_ms]"), "Array<duration>");
}

#[test]
fn unary_neg_recurses_into_operand() {
    assert_eq!(array_type("[-42]"), "Array<int>");
}

#[test]
fn paren_is_transparent() {
    assert_eq!(array_type("[(42)]"), "Array<int>");
}

#[test]
fn binary_over_literals_recurses() {
    assert_eq!(array_type("[42 + 1]"), "Array<int>");
}

#[test]
fn nested_array_recurses() {
    assert_eq!(array_type("[[1, 2]]"), "Array<Array<int>>");
}

#[test]
fn object_with_type_infers_array_of_that_type() {
    let s = array_type_with_decls("type Foo { x: int; }", "[Foo { x: 1 }, Foo { x: 2 }]");
    assert_eq!(s, "Array<Foo>");
}

#[test]
fn generic_object_infers_array_of_instantiated_generic() {
    let s = array_type_with_decls(
        "type Box<T> { x: T; }",
        "[Box<int> { x: 1 }, Box<int> { x: 2 }]",
    );
    assert_eq!(s, "Array<Box<int>>");
}

#[test]
fn object_slot_with_ident_still_infers() {
    // Runtime quirk: the object_expr trigger does NOT recurse into its
    // slots, so an ident slot value doesn't disqualify the array.
    let src = "type Foo { x: int; }\n\
        fn main() {\n    var s = 1;\n    var x = [Foo { x: s }];\n}\n";
    let (uri, pa) = analyze(src);
    assert_eq!(x_type(&pa, &uri), "Array<Foo>");
}

// --- the deliberate deviation from the runtime ---

#[test]
fn binary_time_minus_duration_uses_analyzer_inference() {
    // Runtime mis-infers `[42time - 10s]` as `Array<duration>`. The
    // analyzer's `infer_binary` correctly returns `time` for
    // `time - duration`, so the array types as `Array<time>` here.
    assert_eq!(array_type("[42time - 10s]"), "Array<time>");
}

// --- negative: trigger fails → bare Array<any?> ---

#[test]
fn ident_element_bails_to_bare_array() {
    let src = "fn main() {\n    var s = 1;\n    var x = [s];\n}\n";
    let (uri, pa) = analyze(src);
    assert_eq!(x_type(&pa, &uri), "Array<any?>");
}

#[test]
fn call_element_bails_to_bare_array() {
    let src = "fn make(): int { return 1; }\n\
        fn main() {\n    var x = [make()];\n}\n";
    let (uri, pa) = analyze(src);
    assert_eq!(x_type(&pa, &uri), "Array<any?>");
}

#[test]
fn binary_containing_ident_bails() {
    let src = "fn main() {\n    var s = 1;\n    var x = [42 + s];\n}\n";
    let (uri, pa) = analyze(src);
    assert_eq!(x_type(&pa, &uri), "Array<any?>");
}

#[test]
fn char_literals_bail_runtime_quirk() {
    // Runtime quirk: `['a', 'b']` types as bare `Array`, not
    // `Array<char>`. We mimic.
    assert_eq!(array_type("['a', 'b']"), "Array<any?>");
}

#[test]
fn mixed_types_bail_no_widening() {
    assert_eq!(array_type("[42, 3.14]"), "Array<any?>");
}

#[test]
fn null_mixed_with_typed_bails() {
    assert_eq!(array_type("[3.14, null, 1.337]"), "Array<any?>");
}

#[test]
fn all_null_bails() {
    assert_eq!(array_type("[null]"), "Array<any?>");
    assert_eq!(array_type("[null, null]"), "Array<any?>");
}

#[test]
fn empty_array_bails() {
    assert_eq!(array_type("[]"), "Array<any?>");
}

#[test]
fn divergent_object_types_bail() {
    let src = "type Foo { x: int; }\n\
        type Bar { y: int; }\n\
        fn main() {\n    var x = [Foo { x: 1 }, Bar { y: 1 }];\n}\n";
    let (uri, pa) = analyze(src);
    assert_eq!(x_type(&pa, &uri), "Array<any?>");
}
