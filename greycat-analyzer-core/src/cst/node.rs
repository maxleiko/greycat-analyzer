use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    cst::{
        combi::Either,
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

    pub fn add<T: AddToNode>(&mut self, value: T) {
        value.append_to(self);
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

    pub fn replace_last_token_error(&mut self, kind: ErrorKind) {
        if let Some(n) = self.last_token_mut() {
            *n = match n {
                CstNode::Token(token) => CstNode::Error(NodeError {
                    kind,
                    token: *token,
                }),
                CstNode::Error(error) => CstNode::Error(NodeError {
                    kind,
                    token: error.token,
                }),
                CstNode::Node(_) => unreachable!(),
            }
        }
    }

    fn last_token_mut(&mut self) -> Option<&mut CstNode> {
        self.children.last_mut().and_then(|n| match n {
            CstNode::Node(node) => node.last_token_mut(),
            leaf => Some(leaf),
        })
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
    FnBody,
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
    ModVarDecl,
    Initializer,
    TypeDecl,
    TypeExtends,
    TypeBody,
    EnumDecl,
    TypeAttr,
    TypeMethod,
    StringExpr,
    CallArgs,
    ModPragma,
    EnumBody,
    EnumField,
    ParenExpr,
    PostfixExpr,
    PrefixExpr,
    CallExpr,
    StringIdent,
    NullExpr,
    ThisExpr,
    NumExpr,
    BoolExpr,
    CharExpr,
    NaNExpr,
    InfExpr,
    VarDecl,
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
pub enum ErrorKind {
    UnexpectedSeparator,
    MissingToken,
    UnexpectedToken,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedSeparator => write!(f, "unexpected separator"),
            Self::MissingToken => write!(f, "missing token"),
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

pub trait AddToNode {
    fn append_to(self, node: &mut Node);
}

impl AddToNode for Token {
    fn append_to(self, node: &mut Node) {
        node.children.push(CstNode::Token(self))
    }
}

impl AddToNode for Tokens {
    fn append_to(self, node: &mut Node) {
        self.leading.append_to(node);
        self.token.append_to(node);
    }
}

impl AddToNode for NodeError {
    fn append_to(self, node: &mut Node) {
        node.children.push(CstNode::Error(self))
    }
}

impl<T: AddToNode> AddToNode for Vec<T> {
    fn append_to(self, node: &mut Node) {
        self.into_iter().for_each(|item| item.append_to(node));
    }
}

impl<T: AddToNode, U: AddToNode> AddToNode for Either<T, U> {
    fn append_to(self, node: &mut Node) {
        match self {
            Self::Left(value) => value.append_to(node),
            Self::Right(value) => value.append_to(node),
        }
    }
}

impl<T: AddToNode> AddToNode for Option<T> {
    fn append_to(self, node: &mut Node) {
        if let Some(value) = self {
            value.append_to(node);
        }
    }
}

impl AddToNode for Node {
    fn append_to(self, node: &mut Node) {
        node.children.push(CstNode::Node(self));
    }
}

impl<T: AddToNode, U: AddToNode> AddToNode for Vec<(T, U)> {
    fn append_to(self, node: &mut Node) {
        for (t, u) in self {
            node.add(t);
            node.add(u);
        }
    }
}
