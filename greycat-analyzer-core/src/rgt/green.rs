use bumpalo::{Bump, collections::Vec};

use crate::{cst::NodeKind, span::Span};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

/// Immutable syntax node
#[derive(Debug)]
pub struct GreenNode<'a> {
    pub kind: NodeKind,
    pub children: Vec<'a, &'a GreenNode<'a>>,
    pub span: Span,
    pub id: NodeId,
}

#[derive(Debug)]
pub(crate) struct GreenNodeBuilder<'a> {
    pub kind: NodeKind,
    pub children: Vec<'a, &'a GreenNode<'a>>,
    pub span: Span,
}

/// Interned node handle
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GreenId(usize);

pub struct GreenArena<'a> {
    bump: &'a Bump,
    nodes: Vec<'a, &'a GreenNode<'a>>,
}

impl<'a> GreenArena<'a> {
    pub fn new(bump: &'a Bump) -> Self {
        Self {
            bump,
            nodes: Vec::new_in(bump),
        }
    }

    pub fn alloc(&mut self, builder: GreenNodeBuilder<'a>) -> &GreenNode<'a> {
        // register IDs based on current length
        let id = NodeId(self.nodes.len());
        let node = GreenNode {
            kind: builder.kind,
            children: builder.children,
            span: builder.span,
            id,
        };
        let node = self.bump.alloc(node);
        self.nodes.push(node);
        node
    }

    pub fn get(&self, id: NodeId) -> Option<&'a GreenNode<'a>> {
        self.nodes.get(id.0).copied()
    }
}
