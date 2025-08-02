use core::fmt;
use std::{borrow::Cow, collections::VecDeque};

use serde::Serialize;

use crate::{
    cst::{
        combi::Either,
        display::{DisplayNode, DisplayNodeRule},
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

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum CstNode {
    Node(Node),
    Token(Token),
    Error(NodeError),
}

impl CstNode {
    pub fn span(&self) -> Cow<'_, Span> {
        match self {
            Self::Node(node) => Cow::Owned(node.span()),
            Self::Token(token) => Cow::Borrowed(&token.span),
            Self::Error(NodeError { span, .. }) => Cow::Borrowed(span),
        }
    }

    pub fn start(&self) -> Pos {
        match self {
            Self::Node(node) => node.span().start,
            Self::Token(token) => token.span.start,
            Self::Error(NodeError { span, .. }) => span.start,
        }
    }

    pub fn end(&self) -> Pos {
        match self {
            Self::Node(node) => node.span().end,
            Self::Token(token) => token.span.end,
            Self::Error(NodeError { span, .. }) => span.end,
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    #[serde(rename = "name")]
    pub kind: NodeKind,
    pub children: Vec<CstNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_name: Option<&'static str>,
}

impl Node {
    #[inline(always)]
    pub fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            children: Default::default(),
            field_name: None,
        }
    }

    pub fn field(&mut self, name: &'static str) {
        let _ = self.field_name.replace(name);
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn add<T: AddToNode>(&mut self, value: T) {
        value.append_to(self);
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
        match (self.children.first(), self.children.last()) {
            (None, None) => Span::default(),
            (Some(front), Some(back)) => Span {
                start: front.start(),
                end: back.end(),
            },
            _ => unreachable!(),
        }
    }

    pub fn last_token_mut(&mut self) -> Option<&mut CstNode> {
        self.children.last_mut().and_then(|n| match n {
            CstNode::Node(node) => node.last_token_mut(),
            leaf => Some(leaf),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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
    StaticMemberExpr,
    MemberExpr,
    OffsetExpr,
    NonNullExpr,
    OptionalExpr,
    AsSpec,
    IsSpec,
    ExprStmt,
    LambdaExpr,
    ObjectExpr,
    ObjectFields,
    ObjectFieldEntry,
    ObjectFieldExpr,
    ArrayInlineExpr,
    TemplateExpr,
    Interpolation,
    BinaryExpr,
    BinaryOperator,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeError {
    pub kind: ErrorKind,
    pub span: Span,
}

impl NodeError {
    pub fn got(&self) -> TokenKind {
        match self.kind {
            ErrorKind::ExpectedToken { got, .. } => got,
            ErrorKind::Expected { got, .. } => got,
        }
    }
}

impl From<&NodeError> for lsp_types::Diagnostic {
    fn from(value: &NodeError) -> Self {
        Self {
            range: value.span.to_range(),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            message: value.kind.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "reason")]
pub enum ErrorKind {
    ExpectedToken {
        expected: TokenKind,
        got: TokenKind,
    },
    Expected {
        expected: &'static str,
        got: TokenKind,
    },
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedToken { expected, got } => {
                write!(f, "expected token '{expected}' got '{got}'")
            }
            Self::Expected { expected, got } => write!(f, "expected '{expected}' got '{got}'"),
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

impl<T: AddToNode, U: AddToNode> AddToNode for (T, U) {
    fn append_to(self, node: &mut Node) {
        node.add(self.0);
        node.add(self.1);
    }
}
