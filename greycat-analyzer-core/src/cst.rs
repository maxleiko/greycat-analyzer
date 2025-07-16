mod combi;
pub mod cursor;
pub mod error;
mod expr;
mod parser;
mod stmt;
mod token_ext;

use combi::span_from_nodes;
use cursor::NodeCursor;
pub use parser::*;

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    cst::token_ext::TokenExt,
    lexer::{Token, TokenKind},
    span::{Pos, Span},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CstNode {
    Node(Node),
    Token(Token),
    Error(NodeError),
}

impl CstNode {
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
            Self::Node(node) => node.span(),
            Self::Token(token) => token.span,
            Self::Error(NodeError { token, .. }) => token.span,
        }
    }

    pub fn start(&self) -> Pos {
        match self {
            Self::Node(node) => node.span().start,
            Self::Token(token) => token.span.start,
            Self::Error(NodeError { token, .. }) => token.span.start,
        }
    }

    pub fn end(&self) -> Pos {
        match self {
            Self::Node(node) => node.span().end,
            Self::Token(token) => token.span.end,
            Self::Error(NodeError { token, .. }) => token.span.end,
        }
    }

    pub fn to_display_node<'a>(&'a self, source: &'a str, with_trivia: bool) -> DisplayNode<'a> {
        DisplayNode {
            node: self,
            source,
            with_trivia,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    #[serde(rename = "name")]
    pub kind: NodeKind,
    pub children: Vec<CstNode>,
}

impl Node {
    #[inline(always)]
    pub fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            children: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn add_node(&mut self, node: CstNode) {
        self.children.push(node);
    }

    pub fn add_token_ext(&mut self, token: TokenExt) {
        let TokenExt { leading, token } = token;
        self.add_tokens(leading);
        self.add_token(token);
    }

    pub fn add_token_ext_as_error(&mut self, token: TokenExt, kind: ErrorKind) {
        let TokenExt { leading, token } = token;
        self.add_tokens(leading);
        self.add_node(CstNode::Error(NodeError { kind, token }));
    }

    pub fn add_token(&mut self, token: Token) {
        self.children.push(CstNode::Token(token))
    }

    pub fn add_tokens(&mut self, tokens: Vec<Token>) {
        self.children
            .extend(tokens.into_iter().map(|t| CstNode::Token(t)));
    }

    pub fn cursor(&self) -> NodeCursor<'_> {
        NodeCursor::new(self)
    }

    pub fn find_child_by_rule(&self, rule: NodeKind) -> Option<&CstNode> {
        self.children
            .iter()
            .find(|node| matches!(node, CstNode::Node(node) if node.kind == rule))
    }

    pub fn to_display_node<'a>(
        &'a self,
        source: &'a str,
        with_trivia: bool,
    ) -> DisplayNodeRule<'a> {
        DisplayNodeRule {
            node: self,
            source,
            with_trivia,
        }
    }

    pub fn span(&self) -> Span {
        span_from_nodes(&self.children)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeError {
    pub kind: ErrorKind,
    pub token: Token,
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
pub enum NodeKind {
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

impl From<Token> for CstNode {
    #[inline]
    fn from(value: Token) -> Self {
        Self::Token(value)
    }
}

pub struct DisplayNodeRule<'a> {
    node: &'a Node,
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
        writeln!(f, "{pad}({:?}", self.node.kind)?;
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
    node: &'a CstNode,
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
            CstNode::Node(node) => {
                let node = DisplayNodeRule {
                    node,
                    source: self.source,
                    with_trivia: self.with_trivia,
                };
                node.fmt_node(f, indent)
            }
            CstNode::Token(token) => match token.kind {
                kind @ TokenKind::Ident | kind @ TokenKind::RawString => {
                    let lexeme = &self.source[token.span.as_range(self.source)];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
                kind if self.with_trivia || !kind.is_trivia() => {
                    writeln!(f, "{pad}({kind:?})")
                }
                _ => Ok(()),
            },
            CstNode::Error(NodeError { kind, .. }) => writeln!(f, "{pad}(ERROR \"{kind:?}\")"),
        }
    }
}
