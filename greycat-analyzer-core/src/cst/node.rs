use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    cst::{
        display::{DisplayNode, DisplayNodeRule},
        utils::*,
    },
    lexer::{Token, TokenKind},
    span::{Pos, Span},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub(crate) leading: Vec<Token>,
    pub(crate) token: Token,
}

impl IntoIterator for Tokens {
    type Item = Token;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(mut self) -> Self::IntoIter {
        self.leading.push(self.token);
        self.leading.into_iter()
    }
}

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

    pub fn add_node(&mut self, node: Node) {
        self.children.push(CstNode::Node(node));
    }

    pub fn add_opt_node(&mut self, node: Option<Node>) {
        if let Some(node) = node {
            self.add_node(node);
        }
    }

    pub fn add_error(&mut self, error: NodeError) {
        self.children.push(CstNode::Error(error));
    }

    pub fn add_token(&mut self, token: Token) {
        self.children.push(CstNode::Token(token))
    }

    pub fn add_tokens(&mut self, tokens: Vec<Token>) {
        self.children.extend(tokens.into_iter().map(CstNode::Token));
    }

    pub fn add_tokens2(&mut self, Tokens { leading, token }: Tokens) {
        self.add_tokens(leading);
        self.add_token(token);
    }

    pub fn add_opt_tokens2(&mut self, tokens: Option<Tokens>) {
        if let Some(tokens) = tokens {
            self.add_tokens2(tokens);
        }
    }

    pub fn add_many_tokens(&mut self, items: Vec<Tokens>) {
        for item in items {
            self.add_tokens2(item)
        }
    }

    pub fn find_child_by_kind(&self, kind: NodeKind) -> Option<&CstNode> {
        self.children
            .iter()
            .find(|node| matches!(node, CstNode::Node(node) if node.kind == kind))
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
        self.children.first();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Module,
    Fn,
    Name,
    FnModifiers,
    FnModifier,
    GenericParams,
    GenericParam,
    FnParams,
    FnParam,
    TypeIdent,
    ReturnType,
    Body,
    BodyStmt,
    Pragma,
    Doc,
    PragmaStmt,
    PragmaArgs,
    Expr,
    String,
    Ident,
    TypeDecorator,
    StmtHeader,
    TypeParams,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
