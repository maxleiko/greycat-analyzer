use core::fmt;
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node<'a>>,
    /// Optional token if this is a leaf
    #[serde(skip)]
    pub token: Option<Token>,
    /// Span of the node, covers the span of all children or token
    #[serde(skip)]
    pub span: Span,
}

impl<'a> Node<'a> {
    pub fn to_display_node<'cst, 'src>(
        &'cst self,
        source: &'src str,
    ) -> DisplayNode<'a, 'cst, 'src> {
        DisplayNode { node: self, source }
    }
}

pub struct DisplayNode<'a, 'cst, 'src> {
    node: &'a Node<'cst>,
    source: &'src str,
}

impl<'a, 'cst, 'src> fmt::Display for DisplayNode<'a, 'cst, 'src> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_node(f, self.node, 0)
    }
}

impl<'a, 'cst, 'src> DisplayNode<'a, 'cst, 'src> {
    fn fmt_node(
        &self,
        f: &mut fmt::Formatter<'_>,
        node: &Node<'cst>,
        indent: usize,
    ) -> fmt::Result {
        let pad = "  ".repeat(indent);
        match &node.kind {
            NodeKind::Rule(name) => {
                writeln!(f, "{pad}({name}")?;
                for child in &node.children {
                    self.fmt_node(f, child, indent + 1)?;
                }
                writeln!(f, "{pad})")
            }
            NodeKind::Error(msg) => {
                writeln!(f, "{pad}(Error \"{msg}\")")
            }
            NodeKind::Token(kind) => match kind {
                TokenKind::NewLine(_) | TokenKind::Space(_) => {
                    writeln!(f, "{pad}({kind:?})")
                }
                _ => {
                    let lexeme =
                        &self.source[node.token.as_ref().unwrap().span.as_range(self.source)];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
            },
        }
    }
}

/// Enum to distinguish between composite rules and leaf tokens
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
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
