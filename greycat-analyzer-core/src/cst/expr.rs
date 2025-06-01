use crate::{
    cst::{ErrorKind, Node, Rule},
    lexer::TokenKind,
};

use super::{
    combi::span_from_nodes,
    cst_parser::{CstParser, ParserResult},
    error::ParseError,
};

impl<'src> CstParser<'src> {
    pub(super) fn parse_expr(&mut self, source: &'src str) -> ParserResult<Node> {
        // TODO all other expressions
        let s = self.parse_string(source)?;
        Ok(Node::rule(Rule::Expr, vec![s]))
    }

    pub(super) fn parse_string(&mut self, source: &'src str) -> ParserResult<Node> {
        let mut children = Vec::new();

        let oquote = self.expect(TokenKind::DoubleQuote)?;
        oquote.merge_into(&mut children);

        while let Some(tok) = self.peek() {
            match tok.token.kind {
                TokenKind::DoubleQuote => {
                    let equote = self.next().unwrap();
                    equote.merge_into(&mut children);
                    return Ok(Node::rule(Rule::String, children));
                }
                TokenKind::RawString => {
                    let raw_string = self.next().unwrap();
                    raw_string.merge_into(&mut children);
                }
                TokenKind::EnterInterpolation => {
                    let interpolation = self.parse_interpolation(source)?;
                    children.push(interpolation);
                }
                kind => {
                    let unexpected = self.next().unwrap();
                    unexpected.merge_into_as_error(&mut children, ErrorKind::UnexpectedToken);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }

    pub(super) fn parse_interpolation(&mut self, source: &'src str) -> ParserResult<Node> {
        todo!()
    }
}
