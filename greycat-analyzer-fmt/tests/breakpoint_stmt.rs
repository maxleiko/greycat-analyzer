// P37.4 — `breakpoint;` round-trips byte-identical through the
// formatter. Shape mirrors `break;` / `continue;` (a bare keyword stmt
// emitted as `Doc::text("breakpoint;")`).

use greycat_analyzer_fmt::format;

#[test]
fn standalone_breakpoint_round_trips() {
    let src = "fn f() {\n    breakpoint;\n}\n";
    assert_eq!(format(src), src);
}

#[test]
fn breakpoint_before_return_round_trips() {
    let src = "fn f(): int {\n    breakpoint;\n    return 0;\n}\n";
    assert_eq!(format(src), src);
}

#[test]
fn breakpoint_inside_loop_round_trips() {
    let src = "fn g() {\n    var i = 0;\n    while (i < 3) {\n        breakpoint;\n        i = i + 1;\n    }\n}\n";
    assert_eq!(format(src), src);
}
