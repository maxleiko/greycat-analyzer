use crate::{
    cst::{CstNode, Node, NodeKind}, Token, TokenKind
};

impl Node {
    /// Returns an iterator over child nodes with the given `kind`
    pub fn children_with_kind(&self, kind: NodeKind) -> impl Iterator<Item = &Self> {
        self.children.iter().filter_map(move |child| {
            // match only CstNode::Node with the right kind
            if let CstNode::Node(node) = child
                && node.kind == kind
            {
                return Some(node);
            }
            None
        })
    }

    /// Returns the first node that matches the given `kind`
    pub fn get_node_by_kind(&self, kind: NodeKind) -> Option<&Node> {
        self.children.iter().find_map(|child| match child {
            CstNode::Node(node) if node.kind == kind => Some(node),
            _ => None,
        })
    }

    /// Returns the first node that matches the given `kind`
    pub fn get_node_by_field(&self, name: &str) -> Option<&Node> {
        for child in &self.children {
            if let CstNode::Node(node) = child
                && let Some(field_name) = node.field_name
                && field_name == name
            {
                return Some(node);
            }
        }
        None
    }

    /// Returns all immediate child nodes with the specified `kind`.
    ///
    /// This method only considers direct children and does not recurse into nested nodes.
    pub fn get_nodes_by_kind(&self, kind: NodeKind) -> Vec<&Node> {
        let mut nodes = Vec::new();
        for child in &self.children {
            if let CstNode::Node(node) = child
                && node.kind == kind {
                    nodes.push(node);
                }
        }
        nodes
    }

    /// Returns the first token that matches the given `kind`
    pub fn get_token_by_kind(&self, kind: TokenKind) -> Option<&Token> {
        self.children.iter().find_map(|child| match child {
            CstNode::Token(token) if token.kind == kind => Some(token),
            _ => None,
        })
    }

    /// Returns the first non-trivia token if any
    pub fn first_non_trivia_token(&self) -> Option<&Token> {
        self.children.iter().find_map(|child| match child {
            CstNode::Token(token) if !token.kind.is_trivia() => Some(token),
            _ => None,
        })
    }
}
