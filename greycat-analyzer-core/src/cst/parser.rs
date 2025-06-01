use std::iter::Peekable;

use crate::{
    cst::{Node, NodeKind, NodeRule},
    lexer::{Lexer, Token, TokenKind, tokenize},
};

use super::{error::ParseError, token_ext::TokenExt};

pub(super) type ParserResult<T> = std::result::Result<T, ParseError>;

#[derive(Clone)]
pub struct CstParser<'src> {
    pub(super) lexer: Peekable<Lexer<'src>>,
    pub(super) lookahead: Option<TokenExt>,
}

impl<'src> CstParser<'src> {
    pub fn new(source: &'src str) -> Self {
        let mut parser = Self {
            lexer: Lexer::new(source).peekable(),
            lookahead: None,
        };

        parser.lookahead = parser.bump();
        parser
    }

    pub(super) fn restore(&mut self, bkp: CstParser<'src>) {
        let CstParser { lexer, lookahead } = bkp;
        self.lexer = lexer;
        self.lookahead = lookahead;
    }
}
