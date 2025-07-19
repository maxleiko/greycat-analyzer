use crate::{cst::CstNode, span::Span};

pub(super) fn span_from_nodes(nodes: &[CstNode]) -> Span {
    match (nodes.first(), nodes.last()) {
        (None, None) => Span::default(),
        (Some(first), Some(last)) => Span {
            start: first.start(),
            end: last.end(),
        },
        _ => unreachable!(),
    }
}
