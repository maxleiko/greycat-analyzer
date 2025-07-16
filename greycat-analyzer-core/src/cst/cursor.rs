use crate::{Token, TokenKind};

use super::{CstNode, Node, NodeKind, error::ParseError};

pub struct NodeCursor<'a> {
    nodes: &'a [CstNode],
    index: usize,
}

pub enum Either {
    Left,
    Right,
    None,
}

impl<'a> NodeCursor<'a> {
    pub fn new(rule: &'a Node) -> Self {
        Self {
            nodes: &rule.children,
            index: 0,
        }
    }

    pub fn peek_node(&self) -> Option<&'a CstNode> {
        self.nodes.get(self.index)
    }

    fn next_node(&mut self) -> Option<&'a CstNode> {
        loop {
            match self.nodes.get(self.index)? {
                CstNode::Token(tok) if tok.kind.is_trivia() => {
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
            Some(CstNode::Token(token)) => Ok(token),
            Some(other) => Err(ParseError::Unexpected(other.span())),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub fn expect_rule(&mut self, rule: NodeKind) -> Result<&'a Node, ParseError> {
        match self.next_node() {
            Some(CstNode::Node(node)) => Ok(node),
            Some(other) => Err(ParseError::Unexpected(other.span())),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    pub fn either_token(&mut self, left: TokenKind, right: TokenKind) -> Either {
        match self.peek_node() {
            Some(CstNode::Token(tok)) if tok.kind == left => Either::Left,
            Some(CstNode::Token(tok)) if tok.kind == right => Either::Right,
            _ => Either::None,
        }
    }
}

impl<'a> Iterator for NodeCursor<'a> {
    type Item = &'a CstNode;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_node()
    }
}
