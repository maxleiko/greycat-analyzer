use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
    lexer::{Token, TokenKind},
    span::Span,
};

/// A node in the concrete syntax tree
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node<'a> {
    /// The kind of this node (non-terminal name or token kind wrapped as leaf)
    pub kind: NodeKind<'a>,
    /// Children nodes (only empty if this is a leaf node)
    pub children: Vec<Node<'a>>,
    /// Optional token if this is a leaf
    pub token: Option<Token>,
    /// Span of the node, covers the span of all children or token
    pub span: Span,
}

/// Enum to distinguish between composite rules and leaf tokens
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind<'a> {
    /// A non-terminal symbol (e.g. "Expr", "Stmt", "Block", etc.)
    Rule(Cow<'a, str>),
    /// A terminal token
    Token(TokenKind),
    /// An inline error node (e.g. "missing comma")
    Error(Cow<'a, str>),
}

impl From<Token> for Node<'_> {
    #[inline]
    fn from(value: Token) -> Self {
        let span = value.span;
        Self {
            kind: NodeKind::Token(value.kind),
            children: Vec::new(),
            span,
            token: Some(value),
        }
    }
}
