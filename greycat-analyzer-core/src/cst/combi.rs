use std::convert::Infallible;

use crate::{Token, TokenKind, cst::Tokens};

#[derive(Debug)]
pub enum ParseError {
    Custom(CustomParseError),
    Expected(ExpectedParseError),
    OneOf(OneOfParseError),
}

impl From<CustomParseError> for ParseError {
    #[inline(always)]
    fn from(value: CustomParseError) -> Self {
        Self::Custom(value)
    }
}

impl From<ExpectedParseError> for ParseError {
    #[inline(always)]
    fn from(value: ExpectedParseError) -> Self {
        Self::Expected(value)
    }
}

impl From<OneOfParseError> for ParseError {
    #[inline(always)]
    fn from(value: OneOfParseError) -> Self {
        Self::OneOf(value)
    }
}

#[derive(Debug)]
pub struct CustomParseError {
    pub text: &'static str,
    pub got: Token,
}

#[derive(Debug)]
pub struct ExpectedParseError {
    pub kind: TokenKind,
    pub got: Token,
}

#[derive(Debug)]
pub struct OneOfParseError {
    pub kinds: Vec<Either<TokenKind, &'static str>>,
    pub got: Token,
}

impl From<CustomParseError> for OneOfParseError {
    fn from(value: CustomParseError) -> Self {
        Self {
            kinds: vec![Either::Right(value.text)],
            got: value.got,
        }
    }
}

impl From<ExpectedParseError> for OneOfParseError {
    fn from(value: ExpectedParseError) -> Self {
        Self {
            kinds: vec![Either::Left(value.kind)],
            got: value.got,
        }
    }
}

impl From<ParseError> for OneOfParseError {
    fn from(value: ParseError) -> Self {
        match value {
            ParseError::Custom(err) => OneOfParseError::from(err),
            ParseError::Expected(err) => OneOfParseError::from(err),
            ParseError::OneOf(err) => err,
        }
    }
}

impl ParseError {
    pub fn is_eof(&self) -> bool {
        self.got().kind == TokenKind::Eof
    }

    pub fn got(&self) -> &Token {
        match self {
            Self::Custom(CustomParseError { got, .. }) => got,
            Self::Expected(ExpectedParseError { got, .. }) => got,
            Self::OneOf(OneOfParseError { got, .. }) => got,
        }
    }

    pub fn and(self, other: Self) -> Self {
        let mut acc = OneOfParseError::from(self);
        let got = match other {
            ParseError::Custom(CustomParseError { text, got }) => {
                acc.kinds.push(Either::Right(text));
                got
            }
            ParseError::Expected(ExpectedParseError { kind, got }) => {
                acc.kinds.push(Either::Left(kind));
                got
            }
            ParseError::OneOf(OneOfParseError { kinds, got }) => {
                acc.kinds.extend(kinds);
                got
            }
        };
        if got.span.start.offset < acc.got.span.start.offset {
            acc.got = got;
        }
        Self::OneOf(acc)
    }
}

impl std::iter::Sum for ParseError {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.reduce(|a, b| a.and(b)).unwrap()
    }
}

pub type Res<'t, T, E = ParseError> = std::result::Result<(&'t [Token], T), E>;

pub trait Parser<'t, T, E = ParseError> {
    fn parse(&self, t: &'t [Token]) -> Res<'t, T, E>;
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
    let leading: Vec<Token> = t
        .iter()
        .take_while(|t| t.kind.is_trivia())
        .copied()
        .collect();
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
            Err(ExpectedParseError {
                kind: self.kind,
                got: tok.token,
            }
            .into())
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
            Err(CustomParseError {
                text: self.error_text,
                got: tok.token,
            }
            .into())
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
pub struct OneOf<'p, 't, T> {
    parsers: &'p [&'p dyn Parser<'t, T>],
}

impl<'p, 't: 'p, T> Parser<'t, T> for OneOf<'p, 't, T> {
    fn parse(&self, t: &'t [Token]) -> Res<'t, T, ParseError> {
        let mut acc = Vec::new();
        for parser in self.parsers {
            match parser.parse(t) {
                Ok(res) => return Ok(res),
                Err(err) => {
                    acc.push(err);
                }
            }
        }
        Err(acc.into_iter().sum())
    }
}

pub const fn one_of<'p, 't: 'p, T>(parsers: &'p [&dyn Parser<'t, T>]) -> OneOf<'p, 't, T> {
    OneOf { parsers }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            Err(err2) => Err(err1.and(err2)),
        },
    }
}

#[derive(Clone, Copy)]
pub struct Alt<P1, P2> {
    p1: P1,
    p2: P2,
}

impl<'t, P1, P2, T> Parser<'t, T> for Alt<P1, P2>
where
    P1: Parser<'t, T>,
    P2: Parser<'t, T>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, T, ParseError> {
        match self.p1.parse(t) {
            Ok((t, res)) => Ok((t, res)),
            Err(err1) => match self.p2.parse(t) {
                Ok((t, res)) => Ok((t, res)),
                Err(err2) => Err(err1.and(err2)),
            },
        }
    }
}

pub const fn alt<'t, P1, P2, T>(p1: P1, p2: P2) -> Alt<P1, P2>
where
    P1: Parser<'t, T>,
    P2: Parser<'t, T>,
{
    Alt { p1, p2 }
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

#[derive(Clone, Copy)]
pub struct Many1<P> {
    parser: P,
}

impl<'t, P, T> Parser<'t, Vec<T>> for Many1<P>
where
    P: Parser<'t, T>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, Vec<T>, ParseError> {
        let (t, item) = self.parser.parse(t)?;
        let mut items = vec![item];
        let mut tokens = t;
        while let Ok((t, res)) = self.parser.parse(tokens) {
            items.push(res);
            tokens = t;
        }
        Ok((tokens, items))
    }
}

pub const fn many1<'t, P, T>(parser: P) -> Many1<P>
where
    P: Parser<'t, T>,
{
    Many1 { parser }
}

#[derive(Clone, Copy)]
pub struct Map<P, A, B> {
    parser: P,
    map: fn(A) -> B,
}

impl<'t, P, A, B> Parser<'t, B> for Map<P, A, B>
where
    P: Parser<'t, A>,
{
    fn parse(&self, t: &'t [Token]) -> Res<'t, B, ParseError> {
        match self.parser.parse(t) {
            Ok((t, a)) => Ok((t, (self.map)(a))),
            Err(err) => Err(err),
        }
    }
}

pub const fn map<'t, P, A, B>(parser: P, map: fn(A) -> B) -> Map<P, A, B>
where
    P: Parser<'t, A>,
{
    Map { parser, map }
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

pub fn seq2<'t, P1, P2, T>(p1: P1, p2: P2) -> impl Parser<'t, (T, T)>
where
    P1: Parser<'t, T>,
    P2: Parser<'t, T>,
{
    move |t| {
        let (t, id) = p1.parse(t)?;
        let (t, dc) = p2.parse(t)?;
        Ok((t, (id, dc)))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{span::Pos, span::Span, tokenize};
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
        assert_eq!(tokens.len(), 2);
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
}
