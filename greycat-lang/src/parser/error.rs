use crate::lexer::{Token, TokenKind};

#[derive(Debug)]
pub enum ParseError {
    NoMatch,
    UnexpectedToken(TokenKind, Token),
    ExpectedToken(String, Token),
    ExpectedIdent(String, Token),
    ExpectedIdents(Box<[String]>, Token),
    UnexpectedEof,
}

impl std::error::Error for ParseError {}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO proper display for parse error
        std::fmt::Debug::fmt(self, f)
    }
}
