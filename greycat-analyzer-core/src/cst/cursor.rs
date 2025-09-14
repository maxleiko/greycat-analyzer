use super::{CstNode, Node};

pub struct NodeCursor<'a> {
    nodes: &'a [CstNode<'a>],
    index: usize,
}

pub enum Either {
    Left,
    Right,
    None,
}

impl<'a> NodeCursor<'a> {
    pub fn new(rule: &'a Node) -> Self {
        Self {
            nodes: &rule.children,
            index: 0,
        }
    }

    pub fn peek_node(&self) -> Option<&'a CstNode<'_>> {
        self.nodes.get(self.index)
    }

    fn next_node(&mut self) -> Option<&'a CstNode<'_>> {
        loop {
            match self.nodes.get(self.index)? {
                CstNode::Token(tok) if tok.kind.is_trivia() => {
                    self.index += 1;
                }
                node => {
                    self.index += 1;
                    return Some(node);
                }
            }
        }
    }
}

impl<'a> Iterator for NodeCursor<'a> {
    type Item = &'a CstNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_node()
    }
}
