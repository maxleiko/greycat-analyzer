use std::rc::{Rc, Weak};

use super::green::{GreenNode, NodeId};

pub struct RedNode<'a> {
    /// Reference to the green node (immutable syntax data)
    pub green: &'a GreenNode<'a>,
    /// Weak reference to parent red node (None for root)
    pub parent: Option<Weak<RedNode<'a>>>,
}

impl<'a> RedNode<'a> {
    pub fn id(&self) -> NodeId {
        self.green.id
    }

    pub fn children(&self) -> &[&'a GreenNode<'a>] {
        &self.green.children
    }

    pub fn red_children(self: &Rc<Self>) -> impl Iterator<Item = Rc<RedNode<'a>>> {
        self.green
            .children
            .iter()
            .map(|&child| RedNode::new(child, Some(Rc::downgrade(self))))
    }

    pub fn parent(&self) -> Option<Rc<RedNode<'a>>> {
        self.parent.as_ref().and_then(|weak| weak.upgrade())
    }

    /// Create a red node with given parent
    pub fn new(green: &'a GreenNode<'a>, parent: Option<Weak<RedNode<'a>>>) -> Rc<Self> {
        Rc::new(Self { green, parent })
    }
}
