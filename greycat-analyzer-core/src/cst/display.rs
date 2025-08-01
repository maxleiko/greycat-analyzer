use core::fmt;

use crate::{
    TokenKind,
    cst::{CstNode, Node, NodeError},
};

pub struct DisplayNodeRule<'a> {
    pub(super) node: &'a Node,
    pub(super) source: &'a str,
    pub(super) with_trivia: bool,
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
    pub(super) node: &'a CstNode,
    pub(super) source: &'a str,
    pub(super) with_trivia: bool,
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
                    let lexeme = &self.source[token.span.as_range()];
                    writeln!(f, "{pad}({kind:?} \"{lexeme}\")")
                }
                kind if self.with_trivia || !kind.is_trivia() => {
                    writeln!(f, "{pad}({kind:?})")
                }
                _ => Ok(()),
            },
            CstNode::Error(NodeError { kind, span }) => {
                writeln!(f, "{pad}(ERROR {kind} [{span}])")
            }
        }
    }
}
