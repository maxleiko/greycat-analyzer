use super::*;
use crate::span::*;
use pretty_assertions::assert_eq;

#[test]
fn curly_nl() {
    let tokens = tokenize("{\n}");
    assert_eq!(
        tokens,
        vec![
            Token {
                kind: TokenKind::OpenCurly,
                span: Span {
                    start: Pos::new(0, 0, 0),
                    end: Pos::new(0, 1, 1),
                }
            },
            Token {
                kind: TokenKind::NewLine(1),
                span: Span {
                    start: Pos::new(0, 1, 1),
                    end: Pos::new(1, 0, 2),
                }
            },
            Token {
                kind: TokenKind::CloseCurly,
                span: Span {
                    start: Pos::new(1, 0, 2),
                    end: Pos::new(1, 1, 3),
                }
            },
            Token {
                kind: TokenKind::Eof,
                span: Span {
                    start: Pos::new(1, 1, 3),
                    end: Pos::new(1, 1, 3),
                }
            }
        ]
    );
}

#[test]
fn string_lit() {
    let tokens = tokenize("\"hello world\"");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::DoubleQuote,
            TokenKind::RawString,
            TokenKind::DoubleQuote,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn string_lit_unfinished() {
    let tokens = tokenize("\"hello ");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::DoubleQuote, TokenKind::RawString, TokenKind::Eof]
    );
}

#[test]
fn string_lit_with_interpolation() {
    let tokens = tokenize("\"hello ${world}\"");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::DoubleQuote,
            TokenKind::RawString,
            TokenKind::EnterInterpolation,
            TokenKind::Ident,
            TokenKind::ExitInterpolation,
            TokenKind::DoubleQuote,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn string_lit_with_unfinished_interpolation() {
    let tokens = tokenize("\"hello ${world\"");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::DoubleQuote,
            TokenKind::RawString,
            TokenKind::EnterInterpolation,
            TokenKind::Ident,
            TokenKind::DoubleQuote,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn int_literal() {
    let tokens = tokenize("42");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn float_literal() {
    let tokens = tokenize("3.14");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn float_literal_unfinished() {
    let tokens = tokenize("3.");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn float_literal_too_many_dots() {
    let tokens = tokenize("3.1.4");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn int_literal_with_underscores() {
    let tokens = tokenize("1_000_000");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn explicit_float_literal() {
    let tokens = tokenize("3f");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn explicit_float_literal2() {
    let tokens = tokenize("3_float");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn whitespace() {
    let tokens = tokenize(" \t  ");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Space(4), TokenKind::Eof]
    );
}

#[test]
fn newline() {
    let tokens = tokenize("\n\r\n\n");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::NewLine(3), TokenKind::Eof]
    );
}

#[test]
fn eol_comment() {
    let tokens = tokenize("// hello");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::EolComment, TokenKind::Eof]
    );
}

#[test]
fn block_comment() {
    let tokens = tokenize("/* hello /*\n\n * world */");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::BlockComment, TokenKind::Eof]
    );
}

#[test]
fn block_comment_with_escape() {
    let tokens = tokenize("/* \\* */");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::BlockComment, TokenKind::Eof]
    );
}

#[test]
fn scientific_notation() {
    let tokens = tokenize("1e6");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn scientific_notation_nagative() {
    let tokens = tokenize("1e-6");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Number, TokenKind::Eof]
    );
}

#[test]
fn small_binop() {
    let tokens = tokenize("a <= 42");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![
            TokenKind::Ident,
            TokenKind::Space(1),
            TokenKind::LtEq,
            TokenKind::Space(1),
            TokenKind::Number,
            TokenKind::Eof
        ]
    );
}

#[test]
fn char() {
    let tokens = tokenize("'c'");
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Char { terminated: true }, TokenKind::Eof]
    );
}

#[test]
fn char_escape() {
    let tokens = tokenize(r#"'\\'"#);
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Char { terminated: true }, TokenKind::Eof]
    );
}

#[test]
fn char_unfinished() {
    let tokens = tokenize(r#"'c"#);
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Char { terminated: false }, TokenKind::Eof]
    );
}

#[test]
fn float() {
    let source = "-1.7976931348623157e+308_f";
    let tokens = tokenize(source);
    println!(
        "{:?}",
        tokens
            .iter()
            .map(|t| (t.kind, &source[t.span.as_range()]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        tokens.into_iter().map(|t| t.kind).collect::<Vec<_>>(),
        vec![TokenKind::Minus, TokenKind::Number, TokenKind::Eof]
    );
}
