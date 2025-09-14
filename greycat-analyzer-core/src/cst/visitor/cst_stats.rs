use crate::cst::*;

#[derive(Default)]
pub struct CstStats {
    nodes: usize,
    tokens: usize,
    errors: usize,
}

impl CstVisitor for CstStats {
    fn visit_node_enter(&mut self, node: &Node) -> VisitResult {
        let _ = node;
        self.nodes += 1;
        VisitResult::Continue
    }

    fn visit_node_exit(&mut self, node: &Node) -> VisitResult {
        let _ = node;
        VisitResult::Continue
    }

    fn visit_token(&mut self, token: &crate::Token) -> VisitResult {
        let _ = token;
        self.tokens += 1;
        VisitResult::Continue
    }

    fn visit_error(&mut self, error: &NodeError) -> VisitResult {
        let _ = error;
        self.errors += 1;
        VisitResult::Continue
    }
}

impl std::fmt::Display for CstStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.nodes, self.tokens, self.errors)
    }
}

impl From<&Node<'_>> for CstStats {
    fn from(value: &Node) -> Self {
        let mut stats = Self::default();
        stats.walk(value);
        stats
    }
}
