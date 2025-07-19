use std::convert::Infallible;

use crate::{
    Token, TokenKind,
    cst::Tokens,
    span::{Pos, Span},
    tokenize,
};

#[derive(Debug)]
pub enum ParseError {
    Custom { text: &'static str, got: Token },
    Expected { kind: TokenKind, got: Token },
    OneOf { errors: Vec<ParseError>, got: Token },
}

impl ParseError {
    pub fn is_eof(&self) -> bool {
        let got = match self {
            Self::Custom { got, .. } => got.kind,
            Self::Expected { got, .. } => got.kind,
            Self::OneOf { got, .. } => got.kind,
        };
        got == TokenKind::Eof
    }
}

pub type Res<'a, T, E = ParseError> = std::result::Result<(&'a [Token], T), E>;

pub trait Parser<'a, T, E = ParseError> {
    fn parse(&self, t: &'a [Token]) -> Res<'a, T, E>;
}

impl<'t, T, F, E> Parser<'t, T, E> for F
where
    F: Fn(&'t [Token]) -> Res<'t, T, E>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, T, E> {
        self(t)
    }
}

pub fn peek(mut t: &[Token]) -> (&[Token], Tokens) {
    let mut iter = t.iter();
    let leading: Vec<Token> = iter.take_while(|t| t.kind.is_trivia()).copied().collect();
    t = &t[leading.len()..];
    let token = t[0];
    t = &t[1..];
    (t, Tokens { leading, token })
}

#[derive(Clone, Copy)]
pub struct Matches {
    kind: TokenKind,
}

impl<'t> Parser<'t, Tokens> for Matches {
    fn parse(&self, t: &'t [Token]) -> Res<'t, Tokens, ParseError> {
        let (t, tok) = peek(t);
        if tok.token.kind == self.kind {
            Ok((t, tok))
        } else {
            Err(ParseError::Expected {
                kind: self.kind,
                got: tok.token,
            })
        }
    }
}

#[inline(always)]
pub const fn matches(kind: TokenKind) -> Matches {
    Matches { kind }
}

#[derive(Clone, Copy)]
pub struct MatchesOne<const N: usize> {
    kinds: [TokenKind; N],
    error_text: &'static str,
}

impl<'t, const N: usize> Parser<'t, Tokens> for MatchesOne<N> {
    fn parse(&self, t: &'t [Token]) -> Res<'t, Tokens, ParseError> {
        let (t, tok) = peek(t);
        if self.kinds.contains(&tok.token.kind) {
            Ok((t, tok))
        } else {
            Err(ParseError::Custom {
                text: self.error_text,
                got: tok.token,
            })
        }
    }
}

#[inline(always)]
pub const fn matches_one<const N: usize>(
    kinds: [TokenKind; N],
    error_text: &'static str,
) -> MatchesOne<N> {
    MatchesOne { kinds, error_text }
}

#[derive(Clone, Copy)]
pub struct OneOf<'p, P> {
    parsers: &'p [P],
}

impl<'t, P, T> Parser<'t, T> for OneOf<'static, P>
where
    P: Parser<'t, T>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, T, ParseError> {
        let mut errors = Vec::new();
        for parser in self.parsers {
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

pub const fn one_of<'p, P>(parsers: &'p [P]) -> OneOf<'p, P> {
    OneOf { parsers }
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

pub fn alt<'t, P1, P2, T>(p1: P1, p2: P2) -> impl Parser<'t, T>
where
    P1: Parser<'t, T>,
    P2: Parser<'t, T>,
{
    move |t| match p1.parse(t) {
        Ok((t, res)) => Ok((t, res)),
        Err(err1) => match p2.parse(t) {
            Ok((t, res)) => Ok((t, res)),
            Err(err2) => Err(ParseError::OneOf {
                errors: vec![err1, err2],
                got: t[0],
            }),
        },
    }
}

pub fn seq<'t, P, T>(parsers: &[P]) -> impl Parser<'t, Vec<T>>
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
        Ok((tokens, items))
    }
}

pub fn many<'t, P, T>(parser: P) -> impl Parser<'t, Option<Vec<T>>, Infallible>
where
    P: Parser<'t, T>,
{
    move |t| {
        let mut items = Vec::new();
        let mut tokens = t;
        match parser.parse(tokens) {
            Ok((t, item)) => {
                items.push(item);
                tokens = t;
            }
            Err(_) => return Ok((tokens, None)),
        }
        while let Ok((t, res)) = parser.parse(tokens) {
            items.push(res);
            tokens = t;
        }
        Ok((tokens, Some(items)))
    }
}

pub fn many1<'t, P, T>(parser: P) -> impl Parser<'t, Vec<T>>
where
    P: Parser<'t, T>,
{
    move |t| {
        let (t, item) = parser.parse(t)?;
        let mut items = vec![item];
        let mut tokens = t;
        while let Ok((t, res)) = parser.parse(tokens) {
            items.push(res);
            tokens = t;
        }
        Ok((tokens, items))
    }
}

pub fn map<'t, P, A, B, F>(parser: P, map: F) -> impl Parser<'t, B>
where
    P: Parser<'t, A>,
    F: Fn(A) -> B,
{
    move |t| match parser.parse(t) {
        Ok((t, a)) => Ok((t, map(a))),
        Err(err) => Err(err),
    }
}

pub fn opt<'t, P, T>(parser: P) -> impl Parser<'t, Option<T>, Infallible>
where
    P: Parser<'t, T>,
{
    move |t| match parser.parse(t) {
        Ok((t, res)) => Ok((t, Some(res))),
        Err(_) => Ok((t, None)),
    }
}

pub fn and_then<'t, P, A, B, F>(parser: P, then: F) -> impl Parser<'t, B>
where
    P: Parser<'t, A>,
    F: Fn(A) -> Box<dyn Parser<'t, B>>,
{
    move |t| parser.parse(t).and_then(|(t, res)| then(res).parse(t))
}

pub fn value<'t, T>(value: T) -> impl Parser<'t, T>
where
    T: Clone,
{
    move |t| Ok((t, value.clone()))
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn peeking() {
        let tokens = tokenize(" fn main");
        let (_, tok) = peek(&tokens);
        assert_eq!(tokens.len(), 5);
        assert_eq!(
            tok,
            Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(1),
                    span: Span::new(Pos::new(0, 0, 0), Pos::new(0, 1, 1))
                }],
                token: Token {
                    kind: TokenKind::Fn,
                    span: Span::new(Pos::new(0, 1, 1), Pos::new(0, 3, 3))
                }
            }
        );
    }

    #[test]
    fn all_spaces() {
        let tokens = tokenize("  ");
        let (_, tok) = peek(&tokens);
        assert_eq!(tokens.len(), 3);
        assert_eq!(
            tok,
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
        let (_, kw) = matches(TokenKind::Fn).parse(&tokens).unwrap();
        assert_eq!(kw.token.kind, TokenKind::Fn);
    }

    #[test]
    fn sequence() {
        let tokens = tokenize("fn main() {}");
        let (_, res) = seq(&[matches(TokenKind::Fn), matches(TokenKind::Ident)])
            .parse(&tokens)
            .unwrap();
        assert_eq!(
            res.into_iter().map(|t| t.token.kind).collect::<Vec<_>>(),
            vec![TokenKind::Fn, TokenKind::Ident]
        );
    }
}
