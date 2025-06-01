use crate::{
    cst::{Node, NodeError, NodeKind, NodeRule},
    lexer::TokenKind,
};

use super::{
    combi::span_from_nodes,
    error::ParseError,
    parser::{CstParser, ParserResult},
};

impl<'src> CstParser<'src> {
    pub(super) fn parse_expr(&mut self, source: &'src str) -> ParserResult<Node> {
        // TODO all other expressions
        self.parse_string(source)
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
                    let span = span_from_nodes(&children);
                    let node = Node::Rule {
                        rule: NodeRule::String,
                        children,
                        span,
                    };
                    return Ok(node);
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
                    unexpected.merge_into_as_error(&mut children, NodeError::UnexpectedToken);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }

    pub(super) fn parse_interpolation(&mut self, source: &'src str) -> ParserResult<Node> {
        todo!()
    }
}
