use std::convert::Infallible;

use crate::{
    Token, TokenKind,
    span::{Pos, Span},
    tokenize,
};

#[derive(Debug)]
pub enum ParseError {
    Expected { kind: TokenKind, got: Token },
    OneOf { errors: Vec<ParseError>, got: Token },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tokens {
    leading: Vec<Token>,
    token: Token,
}

pub type Res<'a, T, E = ParseError> = std::result::Result<(&'a [Token], T), E>;

pub trait Parser<'a, T> {
    fn parse(&self, t: &'a [Token]) -> Res<'a, T>;
}

impl<'t, T, F> Parser<'t, T> for F
where
    F: Fn(&'t [Token]) -> Res<'t, T>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, T> {
        self(t)
    }
}

pub fn next(mut t: &[Token]) -> Res<Option<Token>> {
    if t.is_empty() {
        return Ok((t, None));
    }
    let next = t[0];
    Ok((&t[1..], Some(next)))
}

pub fn peek<'t>(mut t: &'t [Token]) -> Res<'t, Tokens, Infallible> {
    let mut iter = t.iter();
    let leading: Vec<Token> = iter.take_while(|t| t.kind.is_trivia()).copied().collect();
    t = &t[leading.len()..];
    let token = t[0];
    Ok((&t[1..], Tokens { leading, token }))
}

pub fn matches<'t>(kind: TokenKind) -> impl Parser<'t, Tokens> {
    move |t| {
        let (t, tok) = peek(t).unwrap();
        if tok.token.kind == kind {
            Ok((t, tok))
        } else {
            Err(ParseError::Expected {
                kind,
                got: tok.token,
            })
        }
    }
}

pub fn one_of<'t, 'p, P, T>(parsers: &'p [P]) -> impl Parser<'t, T>
where
    P: Parser<'t, T>,
{
    move |t| {
        let mut errors = Vec::new();
        for parser in parsers {
            match parser.parse(t) {
                Ok(res) => return Ok(res),
                Err(err) => {
                    errors.push(err);
                }
            }
        }
        Err(ParseError::OneOf { errors, got: t[0] })
    }
}

pub enum Either<L, R> {
    Left(L),
    Right(R),
}

pub fn either<'t, P1, P2, T1, T2>(p1: P1, p2: P2) -> impl Parser<'t, Either<T1, T2>>
where
    P1: Parser<'t, T1>,
    P2: Parser<'t, T2>,
{
    move |t| match p1.parse(t) {
        Ok((t, res)) => Ok((t, Either::Left(res))),
        Err(err1) => match p2.parse(t) {
            Ok((t, res)) => Ok((t, Either::Right(res))),
            Err(err2) => Err(ParseError::OneOf {
                errors: vec![err1, err2],
                got: t[0],
            }),
        },
    }
}

pub fn seq<'t, 'p, P, T>(parsers: &'p [P]) -> impl Parser<'t, Vec<T>>
where
    P: Parser<'t, T>,
{
    move |t| {
        let mut items = Vec::new();
        let mut tokens = t;
        for parser in parsers {
            let (t, res) = parser.parse(tokens)?;
            items.push(res);
            tokens = t;
        }
        Ok((t, items))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn peeking() {
        let tokens = tokenize(" fn main");
        let (t, a) = peek(&tokens).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(
            a,
            Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(1),
                    span: Span::new(Pos::new(0, 0, 0), Pos::new(0, 1, 1))
                }],
                token: Token {
                    kind: TokenKind::Ident,
                    span: Span::new(Pos::new(0, 1, 1), Pos::new(0, 3, 3))
                }
            }
        );
        let (t, b) = peek(t).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(
            b,
            Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(1),
                    span: Span::new(Pos::new(0, 3, 3), Pos::new(0, 4, 4))
                }],
                token: Token {
                    kind: TokenKind::Ident,
                    span: Span::new(Pos::new(0, 4, 4), Pos::new(0, 8, 8))
                }
            }
        );
    }

    #[test]
    fn all_spaces() {
        let tokens = tokenize("  ");
        let (t, a) = peek(&tokens).unwrap();
        assert_eq!(t.len(), 0);
        assert_eq!(
            a,
            Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(2),
                    span: Span::new(Pos::new(0, 0, 0), Pos::new(0, 2, 2))
                }],
                token: Token {
                    kind: TokenKind::Eof,
                    span: Span::new(Pos::new(0, 2, 2), Pos::new(0, 2, 2))
                }
            }
        );
    }

    #[test]
    fn matching() {
        let tokens = tokenize("fn main() {}");
        let (_, kw) = matches(TokenKind::Ident).parse(&tokens).unwrap();
        assert_eq!(kw.token.kind, TokenKind::Ident);
    }

    #[test]
    fn sequence() {
        let tokens = tokenize("fn main() {}");
        let (_, res) = seq(&[matches(TokenKind::Ident), matches(TokenKind::Ident)])
            .parse(&tokens)
            .unwrap();
        assert_eq!(
            res.into_iter().map(|t| t.token.kind).collect::<Vec<_>>(),
            vec![TokenKind::Ident, TokenKind::Ident]
        );
    }
}
