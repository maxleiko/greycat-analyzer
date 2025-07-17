use std::convert::Infallible;

use crate::{
    Token, TokenKind,
    span::{Pos, Span},
    tokenize,
};

pub struct ParseError {
    expected: String,
    got: Token,
}

pub struct Ctx {
    tokens: Vec<Token>,
    curr: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tokens {
    leading: Vec<Token>,
    token: Token,
}

pub type Res<T, E = ParseError> = std::result::Result<(Ctx, T), E>;
pub type Parser<T> = fn(c: Ctx) -> Res<T>;

pub fn next(mut c: Ctx) -> Res<Option<Token>> {
    if c.curr == c.tokens.len() {
        return Ok((c, None));
    }
    let next = c.tokens[c.curr];
    c.curr += 1;
    Ok((c, Some(next)))
}

pub fn peek(mut c: Ctx) -> Res<Option<Tokens>, Infallible> {
    let mut iter = c.tokens[c.curr..].iter();
    let leading: Vec<Token> = iter.take_while(|t| t.kind.is_trivia()).copied().collect();
    c.curr += leading.len();
    let token = c.tokens[c.curr];
    c.curr += 1;
    Ok((c, Some(Tokens { leading, token })))
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn peeking() {
        let tokens = tokenize(" fn main");
        let mut c = Ctx { tokens, curr: 0 };
        let (c, a) = peek(c).unwrap();
        assert_eq!(c.curr, 2);
        assert_eq!(
            a,
            Some(Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(1),
                    span: Span::new(Pos::new(0, 0, 0), Pos::new(0, 1, 1))
                }],
                token: Token {
                    kind: TokenKind::Ident,
                    span: Span::new(Pos::new(0, 1, 1), Pos::new(0, 3, 3))
                }
            })
        );
        let (c, b) = peek(c).unwrap();
        assert_eq!(c.curr, 4);
        assert_eq!(
            b,
            Some(Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(1),
                    span: Span::new(Pos::new(0, 3, 3), Pos::new(0, 4, 4))
                }],
                token: Token {
                    kind: TokenKind::Ident,
                    span: Span::new(Pos::new(0, 4, 4), Pos::new(0, 8, 8))
                }
            })
        );
    }

    #[test]
    fn all_spaces() {
        let tokens = tokenize("  ");
        let mut c = Ctx { tokens, curr: 0 };
        let (c, a) = peek(c).unwrap();
        assert_eq!(c.curr, 2);
        assert_eq!(
            a,
            Some(Tokens {
                leading: vec![Token {
                    kind: TokenKind::Space(2),
                    span: Span::new(Pos::new(0, 0, 0), Pos::new(0, 2, 2))
                }],
                token: Token {
                    kind: TokenKind::Eof,
                    span: Span::new(Pos::new(0, 2, 2), Pos::new(0, 2, 2))
                }
            })
        );
    }
}
