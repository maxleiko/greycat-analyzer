//! P13.3 — typed-suffix numeric literals lower to dedicated
//! [`LiteralKind`] variants instead of bare `Number`.

use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_hir::types::{Decl, Expr, LiteralExpr, LiteralKind, Stmt};
use greycat_analyzer_syntax::parse;

fn first_var_init_kind(src: &str, idx: usize) -> LiteralKind {
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().expect("module lowered");
    let fn_decl = module
        .decls
        .iter()
        .find_map(|d| match &hir.decls[*d] {
            Decl::Fn(f) => Some(f),
            _ => None,
        })
        .expect("fn lowered");
    let body = hir.stmts[fn_decl.body.expect("body")].clone();
    let stmts = match body {
        Stmt::Block(b) => b.stmts,
        _ => panic!("expected block body"),
    };
    let stmt = &hir.stmts[stmts[idx]];
    let init = match stmt {
        Stmt::Var(v) => v.init.expect("init"),
        _ => panic!("expected var stmt"),
    };
    match &hir.exprs[init] {
        Expr::Literal(LiteralExpr { kind, .. }) => *kind,
        other => panic!("expected literal init, got {other:?}"),
    }
}

#[test]
fn time_suffix_lowers_to_time_kind() {
    let src = "fn f() {\n    var t = 100_time;\n}\n";
    assert!(matches!(
        first_var_init_kind(src, 0),
        LiteralKind::Time(100)
    ));
}

#[test]
fn duration_unit_suffix_lowers_to_duration_kind() {
    // 5h → 5 * 3600 * 1e6 µs (GreyCat stores durations in µs).
    let src = "fn f() {\n    var d = 5h;\n}\n";
    let expected_us: i64 = 5 * 3_600 * 1_000_000;
    assert!(matches!(first_var_init_kind(src, 0), LiteralKind::Duration(us) if us == expected_us));
}

#[test]
fn float_suffix_lowers_to_float_kind() {
    let src = "fn f() {\n    var x = 1.5_f;\n}\n";
    assert!(matches!(first_var_init_kind(src, 0), LiteralKind::Float(_)));
}

#[test]
fn map_two_generic_params_lower_both() {
    // Regression: `Map<K, V>` had only `K` captured (P14.9 fix).
    let src = "type T { paths: Map<String, Inner>?; }\ntype Inner {}\n";
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "m", "p", tree.root_node());
    let attr = hir
        .type_attrs
        .iter()
        .find(|(_, a)| &symbols[hir.idents[a.name].symbol] == "paths")
        .map(|(_, a)| a.clone())
        .expect("paths attr");
    let tr = &hir.type_refs[attr.ty.expect("typed")];
    assert_eq!(&symbols[hir.idents[tr.name].symbol], "Map");
    assert_eq!(tr.params.len(), 2, "expected both K and V params");
    let names: Vec<_> = tr
        .params
        .iter()
        .map(|p| &symbols[hir.idents[hir.type_refs[*p].name].symbol])
        .collect();
    assert_eq!(names, vec!["String", "Inner"]);
}

#[test]
fn plain_int_lowers_to_int_kind() {
    let src = "fn f() {\n    var i = 42;\n}\n";
    assert!(matches!(first_var_init_kind(src, 0), LiteralKind::Int(42)));
}

#[test]
fn negated_int_literal_reaches_i64_min() {
    // `-9223372036854775808` is `i64::MIN`. The unary `-` is a
    // separate CST node from the magnitude literal, but the HIR
    // lowering folds the negation into the literal so the
    // magnitude `2^63` is allowed (it's `i64::MIN`'s absolute
    // value, exactly representable as a negated `i64`).
    let src = "fn f() {\n    var i = -9223372036854775808;\n}\n";
    let kind = first_var_init_kind(src, 0);
    assert!(
        matches!(kind, LiteralKind::Int(v) if v == i64::MIN),
        "expected i64::MIN, got {kind:?}",
    );
    // Look up the parse_issue on the literal to confirm the
    // boundary case carries NO overflow flag.
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().unwrap();
    let fnd = module
        .decls
        .iter()
        .find_map(|d| match &hir.decls[*d] {
            Decl::Fn(f) => Some(f),
            _ => None,
        })
        .unwrap();
    let body = hir.stmts[fnd.body.unwrap()].clone();
    let stmts = match body {
        Stmt::Block(b) => b.stmts,
        _ => panic!(),
    };
    let init = match &hir.stmts[stmts[0]] {
        Stmt::Var(v) => v.init.unwrap(),
        _ => panic!(),
    };
    let issue = match &hir.exprs[init] {
        Expr::Literal(l) => l.parse_issue,
        _ => panic!(),
    };
    assert!(
        issue.is_none(),
        "i64::MIN must not flag overflow: {issue:?}"
    );
}

#[test]
fn positive_int_literal_at_i64_max_is_clean() {
    let src = "fn f() {\n    var i = 9223372036854775807;\n}\n";
    assert!(matches!(
        first_var_init_kind(src, 0),
        LiteralKind::Int(v) if v == i64::MAX,
    ));
}

#[test]
fn positive_int_literal_one_past_i64_max_overflows() {
    // `9223372036854775808` is `i64::MIN`'s magnitude — valid as
    // negated, but as a *positive* literal it exceeds `i64::MAX`
    // and must saturate with the overflow flag set.
    let src = "fn f() {\n    var i = 9223372036854775808;\n}\n";
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().unwrap();
    let fnd = module
        .decls
        .iter()
        .find_map(|d| match &hir.decls[*d] {
            Decl::Fn(f) => Some(f),
            _ => None,
        })
        .unwrap();
    let body = hir.stmts[fnd.body.unwrap()].clone();
    let stmts = match body {
        Stmt::Block(b) => b.stmts,
        _ => panic!(),
    };
    let init = match &hir.stmts[stmts[0]] {
        Stmt::Var(v) => v.init.unwrap(),
        _ => panic!(),
    };
    let lit = match &hir.exprs[init] {
        Expr::Literal(l) => l,
        _ => panic!(),
    };
    assert!(matches!(lit.kind, LiteralKind::Int(v) if v == i64::MAX));
    assert!(
        lit.parse_issue.is_some(),
        "magnitude > i64::MAX must overflow"
    );
}

#[test]
fn negated_int_literal_one_past_i64_min_overflows() {
    // `-9223372036854775809` would be `i64::MIN - 1` — out of
    // range even with the negative-asymmetry rule.
    let src = "fn f() {\n    var i = -9223372036854775809;\n}\n";
    let tree = parse(src);
    let symbols = SymbolTable::default();
    let hir = lower_module(src, &symbols, "module", "lib", tree.root_node());
    let module = hir.module.as_ref().unwrap();
    let fnd = module
        .decls
        .iter()
        .find_map(|d| match &hir.decls[*d] {
            Decl::Fn(f) => Some(f),
            _ => None,
        })
        .unwrap();
    let body = hir.stmts[fnd.body.unwrap()].clone();
    let stmts = match body {
        Stmt::Block(b) => b.stmts,
        _ => panic!(),
    };
    let init = match &hir.stmts[stmts[0]] {
        Stmt::Var(v) => v.init.unwrap(),
        _ => panic!(),
    };
    let lit = match &hir.exprs[init] {
        Expr::Literal(l) => l,
        _ => panic!(),
    };
    assert!(matches!(lit.kind, LiteralKind::Int(v) if v == i64::MIN));
    assert!(
        lit.parse_issue.is_some(),
        "magnitude > 2^63 must overflow even when negated",
    );
}

#[test]
fn negated_small_int_literal_folds_cleanly() {
    let src = "fn f() {\n    var i = -42;\n}\n";
    assert!(matches!(first_var_init_kind(src, 0), LiteralKind::Int(-42)));
}

#[test]
fn iso8601_utc_lowers_to_us_since_epoch() {
    // 2024-01-01T00:00:00Z → 1_704_067_200 seconds since Unix epoch
    // → 1_704_067_200_000_000 µs. Grammar wraps ISO literals in `'…'`.
    let src = "fn f() {\n    var t = '2024-01-01T00:00:00Z';\n}\n";
    let expected: i64 = 1_704_067_200 * 1_000_000;
    assert!(matches!(
        first_var_init_kind(src, 0),
        LiteralKind::Iso8601(us) if us == expected
    ));
}

#[test]
fn iso8601_positive_offset_is_subtracted_for_utc() {
    // 2024-01-01T01:00:00+01:00 == 2024-01-01T00:00:00Z.
    let src = "fn f() {\n    var t = '2024-01-01T01:00:00+01:00';\n}\n";
    let expected: i64 = 1_704_067_200 * 1_000_000;
    assert!(matches!(
        first_var_init_kind(src, 0),
        LiteralKind::Iso8601(us) if us == expected
    ));
}

#[test]
fn iso8601_fractional_seconds_truncate_to_microseconds() {
    // .123456789 → 123_456 µs (the trailing 789 is sub-µs).
    let src = "fn f() {\n    var t = '2024-01-01T00:00:00.123456789Z';\n}\n";
    let expected: i64 = 1_704_067_200 * 1_000_000 + 123_456;
    assert!(matches!(
        first_var_init_kind(src, 0),
        LiteralKind::Iso8601(us) if us == expected
    ));
}
