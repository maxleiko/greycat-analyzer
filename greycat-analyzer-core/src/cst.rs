use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    lexer::{Token, TokenKind},
    span::Span,
};

/// A node in the concrete syntax tree
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// The kind of this node (non-terminal name or token kind wrapped as leaf)
    pub kind: NodeKind,
    /// Children nodes (only empty if this is a leaf node)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
    /// Optional token if this is a leaf
    #[serde(skip)]
    pub token: Option<Token>,
    /// Span of the node, covers the span of all children or token
    #[serde(skip)]
    pub span: Span,
}

impl Node {
    pub fn to_display_node<'a>(&'a self, source: &'a str) -> DisplayNode<'a> {
        DisplayNode { node: self, source }
    }
}

/// Enum to distinguish between composite rules and leaf tokens
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NodeKind {
    /// A non-terminal symbol
    Rule(NodeRule),
    /// A terminal token
    Token(TokenKind),
    /// An inline error node (e.g. "missing comma")
    Error(NodeError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NodeRule {
    Module,
    Function,
    FnModifiers,
    GenericParams,
    GenericParam,
    FnParams,
    FnParam,
    TypeIdent,
    ReturnType,
    Body,
    BodyStmt,
    PragmaStmt,
    PragmaArgs,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NodeError {
    UnexpectedSeparator,
    MissingSeparator,
    UnexpectedToken,
}

impl From<Token> for Node {
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

pub struct DisplayNode<'a> {
    node: &'a Node,
    source: &'a str,
}

impl<'a> fmt::Display for DisplayNode<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_node(f, self.node, 0)
    }
}

impl<'a> DisplayNode<'a> {
    fn fmt_node(&self, f: &mut fmt::Formatter<'_>, node: &Node, indent: usize) -> fmt::Result {
        let pad = "  ".repeat(indent);
        match &node.kind {
            NodeKind::Rule(name) => {
                writeln!(f, "{pad}({name:?}")?;
                for child in &node.children {
                    self.fmt_node(f, child, indent + 1)?;
                }
                writeln!(f, "{pad})")
            }
            NodeKind::Error(err) => {
                writeln!(f, "{pad}(Error \"{err:?}\")")
            }
            NodeKind::Token(kind) => match kind {
                TokenKind::Ident | TokenKind::RawString => {
                    let lexeme =
                        &self.source[node.token.as_ref().unwrap().span.as_range(self.source)];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
                _ => {
                    writeln!(f, "{pad}({kind:?})")
                }
            },
        }
    }
}
