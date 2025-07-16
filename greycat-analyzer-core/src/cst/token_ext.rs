use crate::{
    cst::CstNode,
    lexer::{Token, TokenKind},
};

use super::ErrorKind;

/// Used during parsing to collect leading/trailing trivia tokens around a non-trivia token
#[derive(Debug, Clone)]
pub(super) struct TokenExt {
    /// Trivia tokens that appear *before* the main token (eg. spaces, newlines, comments)
    pub leading: Vec<Token>,
    /// The main token itself
    pub token: Token,
    // Trivia tokens that appear *after* the main token (eg. spaces, newlines, comments)
    // pub trailing: Vec<Token>,
}

impl TokenExt {
    pub fn kind(&self) -> TokenKind {
        self.token.kind
    }

    pub fn nb_tokens(&self) -> usize {
        self.leading.len() + 1 /*  + self.trailing.len() */
    }

    pub fn merge_into(self, children: &mut Vec<CstNode>) {
        children.reserve(self.nb_tokens());
        let TokenExt {
            leading,
            token,
            // trailing,
        } = self;
        children.extend(leading.into_iter().map(CstNode::from));
        children.push(CstNode::from(token));
        // children.extend(trailing.into_iter().map(Node::from));
    }

    pub fn merge_into_as<F>(self, children: &mut Vec<CstNode>, as_node: F)
    where
        F: Fn(Token) -> CstNode,
    {
        children.reserve(self.nb_tokens());
        let TokenExt {
            leading,
            token,
            // trailing,
        } = self;
        children.extend(leading.into_iter().map(CstNode::from));
        children.push(as_node(token));
        // children.extend(trailing.into_iter().map(Node::from));
    }

    pub fn merge_into_as_error(self, children: &mut Vec<CstNode>, kind: ErrorKind) {
        children.reserve(self.nb_tokens());
        let TokenExt {
            leading,
            token,
            // trailing,
        } = self;
        children.extend(leading.into_iter().map(CstNode::from));
        children.push(CstNode::error(kind, token));
        // children.extend(trailing.into_iter().map(Node::from));
    }
}
