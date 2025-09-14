use std::{borrow::Cow, convert::Infallible, iter::Sum};

use bumpalo::{
    Bump,
    collections::{CollectIn, Vec as BumpVec},
    vec,
};

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
            kinds: std::vec![Either::Right(value.text)],
            got: value.got,
        }
    }
}

impl From<ExpectedParseError> for OneOfParseError {
    fn from(value: ExpectedParseError) -> Self {
        Self {
            kinds: std::vec![Either::Left(value.kind)],
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

impl std::ops::Add for ParseError {
    type Output = ParseError;

    fn add(self, rhs: Self) -> Self::Output {
        self.and(rhs)
    }
}

pub type Res<'t, 'a, T, E = ParseError> = std::result::Result<(ParserCtx<'t, 'a>, T), E>;

#[derive(Clone, Copy)]
pub struct ParserCtx<'tokens, 'arena> {
    pub arena: &'arena Bump,
    pub tokens: &'tokens [Token],
}

pub trait Parser<'t, 'a, T, E = ParseError> {
    fn name(&self) -> Cow<'static, str> {
        std::any::type_name_of_val(self)
            .rsplit("::")
            .next()
            .unwrap()
            .into()
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, T, E>;
}

impl<'t, 'a, T, F, E> Parser<'t, 'a, T, E> for F
where
    F: Fn(ParserCtx<'t, 'a>) -> Res<'t, 'a, T, E>,
{
    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, T, E> {
        self(ctx)
    }
}

pub fn peek<'t, 'a>(mut ctx: ParserCtx<'t, 'a>) -> (ParserCtx<'t, 'a>, Tokens<'a>) {
    let leading: BumpVec<'a, Token> = ctx
        .tokens
        .iter()
        .take_while(|t| t.kind.is_trivia())
        .copied()
        .collect_in(ctx.arena);
    ctx.tokens = &ctx.tokens[leading.len()..];
    let token = ctx.tokens[0];
    ctx.tokens = &ctx.tokens[1..];
    (ctx, Tokens { leading, token })
}

#[derive(Clone, Copy)]
pub struct Matches {
    kind: TokenKind,
}

impl<'t, 'a> Parser<'t, 'a, Tokens<'a>> for Matches {
    fn name(&self) -> Cow<'static, str> {
        let s = match self.kind {
            TokenKind::EolComment => "eol comment",
            TokenKind::DocComment => "doc",
            TokenKind::BlockComment => "block comment",
            TokenKind::Space(_) => "space",
            TokenKind::NewLine(_) => "newline",
            TokenKind::Ident => "identifier",
            TokenKind::Number => "number",
            TokenKind::Char { .. } => "char",
            TokenKind::Semi => "';'",
            TokenKind::Comma => "','",
            TokenKind::Dot => "'.'",
            TokenKind::DotDot => "'..'",
            TokenKind::OpenParen => "'('",
            TokenKind::CloseParen => "')'",
            TokenKind::OpenCurly => "'{'",
            TokenKind::CloseCurly => "'}'",
            TokenKind::OpenSquare => "'['",
            TokenKind::CloseSquare => "']'",
            TokenKind::AtSign => "'@'",
            TokenKind::Question => "'?'",
            TokenKind::QuestionEq => "'?='",
            TokenKind::QuestionQuestion => "'??'",
            TokenKind::Colon => "':'",
            TokenKind::ColonColon => "'::'",
            TokenKind::Eq => "'='",
            TokenKind::EqEq => "'=='",
            TokenKind::Bang => "'!'",
            TokenKind::BangEq => "'!='",
            TokenKind::BangBang => "'!!'",
            TokenKind::Lt => "'<'",
            TokenKind::LtEq => "'<='",
            TokenKind::Gt => "'>'",
            TokenKind::GtEq => "'>='",
            TokenKind::Minus => "'-'",
            TokenKind::MinusMinus => "'--'",
            TokenKind::Arrow => "'->'",
            TokenKind::AndAnd => "'&&'",
            TokenKind::OrOr => "'||'",
            TokenKind::Plus => "'+'",
            TokenKind::PlusPlus => "'++'",
            TokenKind::Star => "'*'",
            TokenKind::Slash => "'/'",
            TokenKind::Caret => "'^'",
            TokenKind::Percent => "'%'",
            TokenKind::DoubleQuote => "'\"'",
            TokenKind::EnterInterpolation => "'${'",
            TokenKind::ExitInterpolation => "'}'",
            TokenKind::RawString => "raw string",
            TokenKind::Abstract => "'abstract'",
            TokenKind::As => "'as'",
            TokenKind::At => "'at'",
            TokenKind::Break => "'break'",
            TokenKind::Breakpoint => "'breakpoint'",
            TokenKind::Catch => "'catch'",
            TokenKind::Continue => "'continue'",
            TokenKind::Do => "'do'",
            TokenKind::Else => "'else'",
            TokenKind::Enum => "'enum'",
            TokenKind::Extends => "'extends'",
            TokenKind::False => "'false'",
            TokenKind::For => "'for'",
            TokenKind::Fn => "'fn'",
            TokenKind::If => "'if'",
            TokenKind::In => "'in'",
            TokenKind::Is => "'is'",
            TokenKind::Limit => "'limit'",
            TokenKind::Native => "'native'",
            TokenKind::Null => "'null'",
            TokenKind::NaN => "'NaN'",
            TokenKind::Infinity => "'infinity'",
            TokenKind::Private => "'private'",
            TokenKind::Return => "'return'",
            TokenKind::Sampling => "'sampling'",
            TokenKind::Skip => "'skip'",
            TokenKind::Static => "'static'",
            TokenKind::Task => "'task'",
            TokenKind::This => "'this'",
            TokenKind::Throw => "'throw'",
            TokenKind::Try => "'try'",
            TokenKind::Type => "'type'",
            TokenKind::True => "'true'",
            TokenKind::TypeOf => "'typeof'",
            TokenKind::Use => "'use'",
            TokenKind::Var => "'var'",
            TokenKind::While => "'while'",
            TokenKind::Without => "'without'",
            TokenKind::Unknown => "unknown",
            TokenKind::Eof => "EOF",
        };
        Cow::Borrowed(s)
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>, ParseError> {
        let (ctx, tok) = peek(ctx);
        if tok.token.kind == self.kind {
            Ok((ctx, tok))
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

impl<'t, 'a, const N: usize> Parser<'t, 'a, Tokens<'a>> for MatchesOne<N> {
    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Tokens<'a>, ParseError> {
        let (ctx, tok) = peek(ctx);
        if self.kinds.contains(&tok.token.kind) {
            Ok((ctx, tok))
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
pub struct OneOf<'p, 't, 'a, T, E> {
    parsers: &'p [&'p dyn Parser<'t, 'a, T, E>],
}

impl<'p, 't: 'p, 'a: 'p, T, E> Parser<'t, 'a, T, E> for OneOf<'p, 't, 'a, T, E>
where
    E: Sum,
{
    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, T, E> {
        let mut acc = Vec::new();
        for parser in self.parsers {
            match parser.parse(ctx) {
                Ok(res) => return Ok(res),
                Err(err) => {
                    acc.push(err);
                }
            }
        }
        Err(acc.into_iter().sum())
    }
}

pub const fn one_of<'p, 't: 'p, 'a: 'p, T, E>(
    parsers: &'p [&'p dyn Parser<'t, 'a, T, E>],
) -> OneOf<'p, 't, 'a, T, E> {
    OneOf { parsers }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

struct EitherParser<P1, P2> {
    p1: P1,
    p2: P2,
}

impl<'t, 'a, P1, P2, T1, T2> Parser<'t, 'a, Either<T1, T2>> for EitherParser<P1, P2>
where
    P1: Parser<'t, 'a, T1>,
    P2: Parser<'t, 'a, T2>,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("either {} or {}", self.p1.name(), self.p2.name()))
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, Either<T1, T2>> {
        // Try the first parser
        match self.p1.parse(ctx) {
            Ok((ctx, result)) => Ok((ctx, Either::Left(result))),
            Err(err1) => {
                // If first parser fails, try the second parser
                match self.p2.parse(ctx) {
                    Ok((ctx, result)) => Ok((ctx, Either::Right(result))),
                    Err(err2) => Err(err1.and(err2)),
                }
            }
        }
    }
}

#[inline(always)]
pub fn either<'t, 'a, P1, P2, T1, T2>(p1: P1, p2: P2) -> impl Parser<'t, 'a, Either<T1, T2>>
where
    P1: Parser<'t, 'a, T1>,
    P2: Parser<'t, 'a, T2>,
{
    EitherParser { p1, p2 }
}

#[derive(Clone, Copy)]
pub struct Alt<P1, P2> {
    p1: P1,
    p2: P2,
}

impl<'t, 'a, P1, P2, T> Parser<'t, 'a, T> for Alt<P1, P2>
where
    P1: Parser<'t, 'a, T>,
    P2: Parser<'t, 'a, T>,
{
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(format!("{} or {}", self.p1.name(), self.p2.name()))
    }

    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, T> {
        match self.p1.parse(ctx) {
            Ok((ctx, res)) => Ok((ctx, res)),
            Err(err1) => match self.p2.parse(ctx) {
                Ok((ctx, res)) => Ok((ctx, res)),
                Err(err2) => Err(err1.and(err2)),
            },
        }
    }
}

pub const fn alt<'t, 'a, P1, P2, T, E>(p1: P1, p2: P2) -> Alt<P1, P2>
where
    P1: Parser<'t, 'a, T, E>,
    P2: Parser<'t, 'a, T, E>,
{
    Alt { p1, p2 }
}

pub fn many<'t, 'a, P, T, E>(parser: P) -> impl Parser<'t, 'a, Option<BumpVec<'a, T>>, Infallible>
where
    P: Parser<'t, 'a, T, E>,
{
    move |ctx: ParserCtx<'t, 'a>| {
        let mut items = BumpVec::new_in(ctx.arena);
        let mut c = ctx;
        match parser.parse(c) {
            Ok((next, item)) => {
                items.push(item);
                c = next;
            }
            Err(_) => return Ok((c, None)),
        }
        while let Ok((next, res)) = parser.parse(c) {
            items.push(res);
            c = next;
        }
        Ok((c, Some(items)))
    }
}

#[derive(Clone, Copy)]
pub struct Many1<P> {
    parser: P,
}

impl<'t, 'a, P, T, E> Parser<'t, 'a, BumpVec<'a, T>, E> for Many1<P>
where
    P: Parser<'t, 'a, T, E>,
{
    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, BumpVec<'a, T>, E> {
        let (ctx, item) = self.parser.parse(ctx)?;
        let mut items = vec![in ctx.arena; item];
        let mut c = ctx;
        while let Ok((next, res)) = self.parser.parse(c) {
            items.push(res);
            c = next;
        }
        Ok((c, items))
    }
}

pub const fn many1<'t, 'a, P, T, E>(parser: P) -> Many1<P>
where
    P: Parser<'t, 'a, T, E>,
{
    Many1 { parser }
}

#[derive(Clone, Copy)]
pub struct Map<'t, 'a, P, A, B> {
    parser: P,
    map: fn(A, ParserCtx<'t, 'a>) -> B,
}

impl<'t, 'a, P, A, B, E> Parser<'t, 'a, B, E> for Map<'t, 'a, P, A, B>
where
    P: Parser<'t, 'a, A, E>,
{
    fn parse(&self, ctx: ParserCtx<'t, 'a>) -> Res<'t, 'a, B, E> {
        match self.parser.parse(ctx) {
            Ok((ctx, a)) => Ok((ctx, (self.map)(a, ctx))),
            Err(err) => Err(err),
        }
    }
}

pub const fn map<'t, 'a, P, A, B, E>(
    parser: P,
    map: fn(A, ParserCtx<'t, 'a>) -> B,
) -> Map<'t, 'a, P, A, B>
where
    P: Parser<'t, 'a, A, E>,
{
    Map { parser, map }
}

pub fn opt<'t, 'a, P, T>(parser: P) -> impl Parser<'t, 'a, Option<T>, Infallible>
where
    P: Parser<'t, 'a, T>,
{
    move |t| match parser.parse(t) {
        Ok((t, res)) => Ok((t, Some(res))),
        Err(_) => Ok((t, None)),
    }
}

pub fn seq2<'t, 'a, P1, P2, T>(p1: P1, p2: P2) -> impl Parser<'t, 'a, (T, T)>
where
    P1: Parser<'t, 'a, T>,
    P2: Parser<'t, 'a, T>,
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
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize(" fn main"),
        };
        assert_eq!(ctx.tokens.len(), 5);
        let (_, tok) = peek(ctx);
        assert_eq!(
            tok,
            Tokens {
                leading: bumpalo::vec![in &arena; Token {
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
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("  "),
        };
        let (_, tok) = peek(ctx);
        assert_eq!(ctx.tokens.len(), 2);
        assert_eq!(
            tok,
            Tokens {
                leading: bumpalo::vec![in &arena; Token {
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
        let arena = Bump::new();
        let ctx = ParserCtx {
            arena: &arena,
            tokens: &tokenize("fn main() {}"),
        };
        let (_, kw) = matches(TokenKind::Fn).parse(ctx).unwrap();
        assert_eq!(kw.token.kind, TokenKind::Fn);
    }
}
