use crate::{
    Token,
    cst::{CstNode, Node, NodeError, NodeKind},
};

pub trait CstVisitor {
    /// Called when entering a node (before visiting children)
    /// You can match on node.kind to handle specific node types
    fn visit_node_enter(&mut self, node: &Node) -> VisitResult {
        let _ = node;
        VisitResult::Continue
    }

    /// Called when exiting a node (after visiting children)
    /// Useful for post-order processing
    fn visit_node_exit(&mut self, node: &Node) -> VisitResult {
        let _ = node;
        VisitResult::Continue
    }

    /// Called when visiting a token
    fn visit_token(&mut self, token: &Token) -> VisitResult {
        let _ = token;
        VisitResult::Continue
    }

    /// Called when visiting an error node
    fn visit_error(&mut self, error: &NodeError) -> VisitResult {
        let _ = error;
        VisitResult::Continue
    }
}

pub trait CstVisitorMut {
    /// Called when entering a node (before visiting children)
    /// You can match on node.kind to handle specific node types
    fn visit_node_enter(&mut self, node: &mut Node) -> VisitResult {
        let _ = node;
        VisitResult::Continue
    }

    /// Called when exiting a node (after visiting children)
    /// Useful for post-order transformations like flattening
    fn visit_node_exit(&mut self, node: &mut Node) -> VisitResult {
        let _ = node;
        VisitResult::Continue
    }

    /// Called when visiting a token
    fn visit_token(&mut self, token: &mut Token) -> VisitResult {
        let _ = token;
        VisitResult::Continue
    }

    /// Called when visiting an error node
    fn visit_error(&mut self, error: &mut NodeError) -> VisitResult {
        let _ = error;
        VisitResult::Continue
    }
}

/// Controls how the visitor should proceed
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VisitResult {
    /// Continue visiting normally
    Continue,
    /// Skip visiting the children of the current node
    SkipChildren,
    /// Stop visiting entirely
    Stop,
}

/// Extension trait to add visiting capability to CstNode
pub trait Visitable {
    fn accept<V: CstVisitor>(&self, visitor: &mut V) -> VisitResult;
}

impl Visitable for CstNode {
    fn accept<V: CstVisitor>(&self, visitor: &mut V) -> VisitResult {
        match self {
            CstNode::Node(node) => visit_node(visitor, node),
            CstNode::Token(token) => visitor.visit_token(token),
            CstNode::Error(error) => visitor.visit_error(error),
        }
    }
}

fn visit_node<V: CstVisitor>(visitor: &mut V, node: &Node) -> VisitResult {
    match visitor.visit_node_enter(node) {
        VisitResult::Stop => VisitResult::Stop,
        VisitResult::SkipChildren => {
            // Skip children but still call exit
            visitor.visit_node_exit(node)
        }
        VisitResult::Continue => {
            // Visit all children
            for child in &node.children {
                let child_result = child.accept(visitor);
                if child_result == VisitResult::Stop {
                    return VisitResult::Stop;
                }
            }

            // Call exit method after children
            visitor.visit_node_exit(node)
        }
    }
}

/// Convenience function to walk a CST tree
pub fn walk_cst<V: CstVisitor>(root: &Node, visitor: &mut V) {
    let _ = visit_node(visitor, root);
}

/// Extension trait to add mutable visiting capability to CstNode
pub trait VisitableMut {
    fn accept_mut<V: CstVisitorMut>(&mut self, visitor: &mut V) -> VisitResult;
}

impl VisitableMut for CstNode {
    fn accept_mut<V: CstVisitorMut>(&mut self, visitor: &mut V) -> VisitResult {
        match self {
            CstNode::Node(node) => visit_node_mut(visitor, node),
            CstNode::Token(token) => visitor.visit_token(token),
            CstNode::Error(error) => visitor.visit_error(error),
        }
    }
}

fn visit_node_mut<V: CstVisitorMut>(visitor: &mut V, node: &mut Node) -> VisitResult {
    match visitor.visit_node_enter(node) {
        VisitResult::Stop => VisitResult::Stop,
        VisitResult::SkipChildren => {
            // Skip children but still call exit
            visitor.visit_node_exit(node)
        }
        VisitResult::Continue => {
            // Visit all children
            for child in &mut node.children {
                let child_result = child.accept_mut(visitor);
                if child_result == VisitResult::Stop {
                    return VisitResult::Stop;
                }
            }

            // Call exit method after children
            visitor.visit_node_exit(node)
        }
    }
}

/// Convenience function to walk a CST tree
pub fn walk_cst_mut<V: CstVisitorMut>(root: &mut Node, visitor: &mut V) {
    let _ = visit_node_mut(visitor, root);
}

// Example visitor that handles specific node kinds
#[derive(Default)]
pub struct FunctionFinder {
    pub function_names: Vec<String>,
    pub function_count: usize,
}

impl CstVisitor for FunctionFinder {
    fn visit_node_enter(&mut self, node: &Node) -> VisitResult {
        match node.kind {
            NodeKind::FnDecl => {
                self.function_count += 1;
                // Look for identifier in children to get function name
                for child in &node.children {
                    if let CstNode::Node(child_node) = child {
                        if matches!(child_node.kind, NodeKind::Ident) {
                            // In a real implementation, you'd extract the actual name
                            self.function_names
                                .push(format!("function_{}", self.function_count));
                            break;
                        }
                    }
                }
                VisitResult::Continue
            }
            // Skip visiting inside expressions for performance
            NodeKind::Expr => VisitResult::SkipChildren,
            _ => VisitResult::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_finder() {
        // Create a test tree with function nodes
        let root = Node {
            kind: NodeKind::Module,
            field_name: None,
            children: vec![
                CstNode::Node(Node {
                    kind: NodeKind::FnDecl,
                    field_name: None,
                    children: vec![CstNode::Node(Node {
                        kind: NodeKind::Ident,
                        field_name: Some("name"),
                        children: vec![],
                    })],
                }),
                CstNode::Node(Node {
                    kind: NodeKind::FnDecl,
                    field_name: None,
                    children: vec![],
                }),
            ],
        };

        let mut finder = FunctionFinder::default();
        walk_cst(&root, &mut finder);

        assert_eq!(finder.function_count, 2);
        assert_eq!(finder.function_names.len(), 1); // Only one had an identifier child
    }
}
