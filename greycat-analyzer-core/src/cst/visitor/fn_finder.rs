use crate::cst::*;

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
                    if let CstNode::Node(child_node) = child
                        && matches!(child_node.kind, NodeKind::Ident)
                    {
                        // In a real implementation, you'd extract the actual name
                        self.function_names
                            .push(format!("function_{}", self.function_count));
                        break;
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

impl From<&Node<'_>> for FunctionFinder {
    fn from(value: &Node) -> Self {
        let mut stats = Self::default();
        stats.walk(value);
        stats
    }
}

#[test]
fn test_function_finder() {
    let arena = Bump::new();
    // Create a test tree with function nodes
    let root = Node {
        kind: NodeKind::Module,
        field_name: None,
        children: bumpalo::vec![in &arena;
            CstNode::Node(Node {
                kind: NodeKind::FnDecl,
                field_name: None,
                children: bumpalo::vec![in &arena; CstNode::Node(Node {
                    kind: NodeKind::Ident,
                    field_name: Some("name"),
                    children: bumpalo::vec![in &arena],
                })],
            }),
            CstNode::Node(Node {
                kind: NodeKind::FnDecl,
                field_name: None,
                children: bumpalo::vec![in &arena],
            }),
        ],
    };

    let finder = FunctionFinder::from(&root);

    assert_eq!(finder.function_count, 2);
    assert_eq!(finder.function_names.len(), 1); // Only one had an identifier child
}
