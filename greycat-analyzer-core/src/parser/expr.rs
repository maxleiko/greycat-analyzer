use crate::{
    cst::{Node, NodeError, NodeKind, NodeRule},
    lexer::TokenKind,
};

use super::{Parser, ParserResult, error::ParseError, span_from_nodes};

impl<'src> Parser<'src> {
    pub fn parse_expr(&mut self, source: &'src str) -> ParserResult<Node> {
        // TODO all other expressions
        self.parse_string(source)
    }

    pub fn parse_string(&mut self, source: &'src str) -> ParserResult<Node> {
        let mut children = Vec::new();

        let oquote = self.expect(TokenKind::DoubleQuote)?;
        oquote.merge_into(&mut children);

        while let Some(tok) = self.peek() {
            match tok.token.kind {
                TokenKind::DoubleQuote => {
                    let equote = self.next().unwrap();
                    equote.merge_into(&mut children);
                    let span = span_from_nodes(&children);
                    return Ok(Node {
                        kind: NodeKind::Rule(NodeRule::String),
                        children,
                        token: None,
                        span,
                    });
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
                    unexpected.merge_into_as(&mut children, |tok| Node {
                        kind: NodeKind::Error(NodeError::UnexpectedToken),
                        children: Vec::new(),
                        span: tok.span,
                        token: Some(tok),
                    });
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }

    pub fn parse_interpolation(&mut self, source: &'src str) -> ParserResult<Node> {
        todo!()
    }
}
