mod combi;
pub mod error;
mod expr;
mod parser;
mod stmt;
mod token_ext;

pub use parser::*;

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    lexer::{Token, TokenKind},
    span::{Pos, Span},
};

/// A node in the concrete syntax tree
// #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// pub struct Node2 {
//     /// The kind of this node (non-terminal name or token kind wrapped as leaf)
//     pub kind: NodeKind,
//     /// Children nodes (only empty if this is a leaf node)
//     #[serde(skip_serializing_if = "Vec::is_empty")]
//     pub children: Vec<Node>,
//     /// Optional token if this is a leaf
//     #[serde(skip)]
//     pub token: Option<Token>,
//     /// Span of the node, covers the span of all children or token
//     #[serde(skip)]
//     pub span: Span,
// }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Rule {
        rule: NodeRule,
        children: Vec<Node>,
        span: Span,
    },
    Token(Token),
    Error {
        kind: NodeError,
        token: Token,
    },
}

impl Node {
    pub fn span(&self) -> Span {
        match self {
            Self::Rule { span, .. } => *span,
            Self::Token(token) => token.span,
            Self::Error { token, .. } => token.span,
        }
    }

    pub fn start(&self) -> Pos {
        match self {
            Self::Rule { span, .. } => span.start,
            Self::Token(token) => token.span.start,
            Self::Error { token, .. } => token.span.start,
        }
    }

    pub fn end(&self) -> Pos {
        match self {
            Self::Rule { span, .. } => span.end,
            Self::Token(token) => token.span.end,
            Self::Error { token, .. } => token.span.end,
        }
    }

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
    Name,
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

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedSeparator => write!(f, "unexpected separator"),
            Self::MissingSeparator => write!(f, "missing separator"),
            Self::UnexpectedToken => write!(f, "unexpected token"),
        }
    }
}

impl From<Token> for Node {
    #[inline]
    fn from(value: Token) -> Self {
        Self::Token(value)
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
        match node {
            Node::Rule {
                rule,
                children,
                span,
            } => {
                writeln!(f, "{pad}({rule:?}")?;
                for child in children {
                    self.fmt_node(f, child, indent + 1)?;
                }
                writeln!(f, "{pad})")
            }
            Node::Token(token) => match token.kind {
                kind @ TokenKind::Ident | kind @ TokenKind::RawString => {
                    let lexeme = &self.source[token.span.as_range(self.source)];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
                kind  if !kind.is_trivia() => {
                    writeln!(f, "{pad}({kind:?})")
                }
                _ => Ok(())
            },
            Node::Error { kind, .. } => writeln!(f, "{pad}(ERROR \"{kind:?}\")"),
        }
    }
}
