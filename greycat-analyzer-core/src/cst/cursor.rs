use crate::{Token, TokenKind};

use super::{Node, NodeRule, Rule, error::ParseError};

pub struct NodeCursor<'a> {
    nodes: &'a [Node],
    index: usize,
}

impl<'a> NodeCursor<'a> {
    pub fn new(rule: &'a NodeRule) -> Self {
        Self {
            nodes: &rule.children,
            index: 0,
        }
    }

    fn peek_node(&self) -> Option<&'a Node> {
        self.nodes.get(self.index)
    }

    fn next_node(&mut self) -> Option<&'a Node> {
        loop {
            match self.nodes.get(self.index)? {
                Node::Token(tok) if tok.kind.is_trivia() => {
                    self.index += 1;
                }
                node => {
                    self.index += 1;
                    return Some(node);
                }
            }
        }
    }

    pub fn expect_token(&mut self, kind: TokenKind) -> Result<&'a Token, ParseError> {
        match self.next_node() {
            Some(Node::Token(token)) => Ok(token),
            Some(other) => Err(ParseError::Unexpected(other.span())),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub fn expect_rule(&mut self, rule: Rule) -> Result<&'a NodeRule, ParseError> {
        match self.next_node() {
            Some(Node::Rule(node)) => Ok(node),
            Some(other) => Err(ParseError::Unexpected(other.span())),
            None => Err(ParseError::UnexpectedEof),
        }
    }
}

impl<'a> Iterator for NodeCursor<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_node()
    }
}
