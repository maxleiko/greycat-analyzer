mod combi;
pub mod cursor;
pub mod error;
mod expr;
mod cst_parser;
mod stmt;
mod token_ext;

use combi::span_from_nodes;
use cursor::NodeCursor;
pub use cst_parser::*;

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    lexer::{Token, TokenKind},
    span::{Pos, Span},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Rule(NodeRule),
    Token(Token),
    Error(NodeError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRule {
    pub rule: Rule,
    pub children: Vec<Node>,
    pub span: Span,
}

impl NodeRule {
    #[inline(always)]
    pub fn new(rule: Rule, children: Vec<Node>) -> Self {
        let span = span_from_nodes(&children);
        Self {
            rule,
            children,
            span,
        }
    }

    pub fn cursor(&self) -> NodeCursor<'_> {
        NodeCursor::new(self)
    }

    pub fn find_child_by_rule(&self, rule: Rule) -> Option<&Node> {
        self.children
            .iter()
            .find(|node| matches!(node, Node::Rule(node) if node.rule == rule))
    }

    pub fn to_display_node<'a>(&'a self, source: &'a str) -> DisplayNodeRule<'a> {
        // TODO configure 'with_trivia' from command-line
        DisplayNodeRule {
            node: self,
            source,
            with_trivia: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeError {
    pub kind: ErrorKind,
    pub token: Token,
}

impl Node {
    #[inline(always)]
    pub fn rule(rule: Rule, children: Vec<Node>) -> Self {
        Self::Rule(NodeRule::new(rule, children))
    }

    #[inline(always)]
    pub fn token(token: Token) -> Self {
        Self::Token(token)
    }

    #[inline(always)]
    pub fn error(kind: ErrorKind, token: Token) -> Self {
        Self::Error(NodeError { kind, token })
    }

    pub fn span(&self) -> Span {
        match self {
            Self::Rule(NodeRule { span, .. }) => *span,
            Self::Token(token) => token.span,
            Self::Error(NodeError { token, .. }) => token.span,
        }
    }

    pub fn start(&self) -> Pos {
        match self {
            Self::Rule(NodeRule { span, .. }) => span.start,
            Self::Token(token) => token.span.start,
            Self::Error(NodeError { token, .. }) => token.span.start,
        }
    }

    pub fn end(&self) -> Pos {
        match self {
            Self::Rule(NodeRule { span, .. }) => span.end,
            Self::Token(token) => token.span.end,
            Self::Error(NodeError { token, .. }) => token.span.end,
        }
    }

    pub fn to_display_node<'a>(&'a self, source: &'a str) -> DisplayNode<'a> {
        // TODO configure 'with_trivia' from command-line
        DisplayNode {
            node: self,
            source,
            with_trivia: true,
        }
    }
}

impl From<&NodeError> for lsp_types::Diagnostic {
    fn from(value: &NodeError) -> Self {
        Self {
            range: value.token.span.to_range(),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: value.kind.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Rule {
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
    Expr,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ErrorKind {
    UnexpectedSeparator,
    MissingSeparator,
    UnexpectedToken,
}

impl std::fmt::Display for ErrorKind {
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

pub struct DisplayNodeRule<'a> {
    node: &'a NodeRule,
    source: &'a str,
    with_trivia: bool,
}

impl<'a> fmt::Display for DisplayNodeRule<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_node(f, 0)
    }
}

impl<'a> DisplayNodeRule<'a> {
    fn fmt_node(&self, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        let pad = "  ".repeat(indent);
        writeln!(f, "{pad}({:?}", self.node.rule)?;
        for child in &self.node.children {
            let child = DisplayNode {
                node: child,
                source: self.source,
                with_trivia: self.with_trivia,
            };
            child.fmt_node(f, indent + 1)?;
        }
        writeln!(f, "{pad})")
    }
}

pub struct DisplayNode<'a> {
    node: &'a Node,
    source: &'a str,
    with_trivia: bool,
}

impl<'a> fmt::Display for DisplayNode<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_node(f, 0)
    }
}

impl<'a> DisplayNode<'a> {
    fn fmt_node(&self, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        let pad = "  ".repeat(indent);
        match self.node {
            Node::Rule(node) => {
                let node = DisplayNodeRule {
                    node,
                    source: self.source,
                    with_trivia: self.with_trivia,
                };
                node.fmt_node(f, indent)
            }
            Node::Token(token) => match token.kind {
                kind @ TokenKind::Ident | kind @ TokenKind::RawString => {
                    let lexeme = &self.source[token.span.as_range(self.source)];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
                kind if self.with_trivia || !kind.is_trivia() => {
                    writeln!(f, "{pad}({kind:?})")
                }
                _ => Ok(()),
            },
            Node::Error(NodeError { kind, .. }) => writeln!(f, "{pad}(ERROR \"{kind:?}\")"),
        }
    }
}
