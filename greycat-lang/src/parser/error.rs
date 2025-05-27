use crate::lexer::{Token, TokenKind};

pub enum ParseError {
    UnexpectedToken(TokenKind, Token),
    ExpectedToken(String, Token),
    ExpectedIdent(String, Token),
    UnexpectedEof,
}
