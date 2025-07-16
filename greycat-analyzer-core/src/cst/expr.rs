use crate::{
    Node, NodeError,
    cst::{CstNode, ErrorKind, NodeKind, TokenExt},
    lexer::TokenKind,
};

use super::{
    combi::span_from_nodes,
    error::ParseError,
    parser::{CstParser, ParserResult},
};

impl<'src> CstParser<'src> {
    pub(super) fn parse_expr(&mut self, source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::Expr);
        // TODO all other expressions
        let s = self.parse_string(source)?;
        node.add_node(s);
        Ok(CstNode::Node(node))
    }

    pub(super) fn parse_string(&mut self, source: &'src str) -> ParserResult<CstNode> {
        let mut node = Node::new(NodeKind::String);

        let oquote = self.expect(TokenKind::DoubleQuote)?;
        node.add_token_ext(oquote);

        while let Some(tok) = self.peek() {
            match tok.token.kind {
                TokenKind::DoubleQuote => {
                    let equote = self.next().unwrap();
                    node.add_token_ext(equote);
                    return Ok(CstNode::Node(node));
                }
                TokenKind::RawString => {
                    let raw_string = self.next().unwrap();
                    // TODO we might need to wrap this raw_string into its own Node here, maybe..
                    node.add_token_ext(raw_string);
                }
                TokenKind::EnterInterpolation => {
                    let interpolation = self.parse_interpolation(source)?;
                    node.add_node(interpolation);
                }
                kind => {
                    let unexpected = self.next().unwrap();
                    node.add_token_ext_as_error(unexpected, ErrorKind::UnexpectedToken);
                }
            }
        }
        Err(ParseError::UnexpectedEof)
    }

    pub(super) fn parse_interpolation(&mut self, source: &'src str) -> ParserResult<CstNode> {
        todo!()
    }
}
