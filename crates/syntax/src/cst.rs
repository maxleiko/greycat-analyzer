// P1.5
//! Small utility surface over tree-sitter CST nodes.
//!
//! Replaces the retired `cursor.rs` / `node_query.rs` helpers from the
//! old hand-rolled CST. Everything here is a thin extension method on
//! `tree_sitter::Node`; the goal is to keep call sites in the LSP and
//! analyzer crates terse without wrapping nodes in another type.

use std::ops::Range;

use tree_sitter::{Node, TreeCursor};

/// Find the deepest named node whose byte range contains `offset`. If no
/// named node matches, returns `None` (e.g. `offset` past EOF).
pub fn node_at_offset<'tree>(root: Node<'tree>, offset: usize) -> Option<Node<'tree>> {
    if offset > root.end_byte() {
        return None;
    }
    let mut current = root.descendant_for_byte_range(offset, offset)?;
    // descendant_for_byte_range may return an anonymous (punctuation) node
    // — walk up until we land on something named.
    while !current.is_named() {
        current = current.parent()?;
    }
    Some(current)
}

/// Iterator over a node and all its ancestors (innermost first), stopping
/// when there's no parent left.
pub fn ancestors(node: Node<'_>) -> Ancestors<'_> {
    Ancestors {
        current: Some(node),
    }
}

pub struct Ancestors<'tree> {
    current: Option<Node<'tree>>,
}

impl<'tree> Iterator for Ancestors<'tree> {
    type Item = Node<'tree>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.current?;
        self.current = n.parent();
        Some(n)
    }
}

/// All children of `node` accessible through field `name`. Tree-sitter's
/// `children_by_field_name` returns an iterator borrowing a cursor; this
/// wrapper hides the cursor allocation.
pub fn children_by_field<'tree>(node: Node<'tree>, name: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.children_by_field_name(name, &mut cursor).collect()
}

/// Source text covered by `node`, or `""` if the byte range is invalid
/// for `source` (shouldn't happen with a tree parsed from `source`, but
/// keeps the API total).
pub fn text_of<'src>(node: Node<'_>, source: &'src str) -> &'src str {
    source.get(node.byte_range()).unwrap_or("")
}

/// Walk every named descendant of `root` in pre-order, calling `visit`
/// on each. Returns `false` from `visit` to skip a sub-tree (the visitor
/// won't descend into the children of the node that returned false).
pub fn walk_named<'tree, F>(root: Node<'tree>, mut visit: F)
where
    F: FnMut(Node<'tree>) -> bool,
{
    fn rec<'tree, F: FnMut(Node<'tree>) -> bool>(
        node: Node<'tree>,
        cursor: &mut TreeCursor<'tree>,
        visit: &mut F,
    ) {
        if !node.is_named() {
            return;
        }
        if !visit(node) {
            return;
        }
        let snapshot_id = node.id();
        if !cursor.goto_first_child() {
            return;
        }
        loop {
            rec(cursor.node(), cursor, visit);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
        debug_assert_eq!(cursor.node().id(), snapshot_id);
    }

    let mut cursor = root.walk();
    rec(root, &mut cursor, &mut visit);
}

/// Returns `(opt_chaining, post_optional)` from the `optional`
/// siblings of the property at `prop_id`: `opt_chaining` when one sits
/// before (`a?.b`), `post_optional` when one sits after (`a.b?`).
pub fn optional_flags_around(
    node: tree_sitter::Node<'_>,
    prop_id: usize,
) -> (Option<Range<usize>>, Option<Range<usize>>) {
    let mut cursor = node.walk();
    let mut pre: Option<Range<usize>> = None;
    let mut post: Option<Range<usize>> = None;
    let mut seen_prop = false;
    for c in node.named_children(&mut cursor) {
        if c.id() == prop_id {
            seen_prop = true;
            continue;
        }
        if c.kind() == "optional" {
            if seen_prop {
                post = Some(c.byte_range());
            } else {
                pre = Some(c.byte_range());
            }
        }
    }
    (pre, post)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn node_at_offset_descends_to_innermost_named() {
        let src = "fn greet(name: String): String { return name; }\n";
        let tree = parse(src);
        let offset = src.find("greet").unwrap();
        let node = node_at_offset(tree.root_node(), offset).expect("hit a named node");
        assert_eq!(node.kind(), "ident");
        assert_eq!(text_of(node, src), "greet");
    }

    #[test]
    fn node_at_offset_past_eof_returns_none() {
        let tree = parse("fn a() {}\n");
        assert!(node_at_offset(tree.root_node(), 9999).is_none());
    }

    #[test]
    fn ancestors_iterates_to_root() {
        let src = "fn a() { return 1; }\n";
        let tree = parse(src);
        let offset = src.find("return").unwrap() + 7; // inside the `1`
        let node = node_at_offset(tree.root_node(), offset).unwrap();
        let kinds: Vec<&str> = ancestors(node).map(|n| n.kind()).collect();
        assert!(kinds.contains(&"number"));
        assert!(kinds.contains(&"return_stmt"));
        assert!(kinds.contains(&"block"));
        assert!(kinds.contains(&"fn_decl"));
        assert!(kinds.contains(&"module"));
        assert_eq!(*kinds.last().unwrap(), "module");
    }

    #[test]
    fn children_by_field_returns_field_matches() {
        let src = "fn greet(name: String, age: int): bool { return true; }\n";
        let tree = parse(src);
        let fn_decl = tree
            .root_node()
            .child(0)
            .expect("module has a child")
            .child_by_field_name("params")
            .map(|p| {
                let mut c = p.walk();
                p.named_children(&mut c)
                    .find(|c| c.kind() == "fn_param")
                    .unwrap()
                    .parent()
                    .unwrap()
            });
        assert!(fn_decl.is_some());

        let params = tree
            .root_node()
            .child(0)
            .unwrap()
            .child_by_field_name("params")
            .unwrap();
        // fn_params has positional `param` field for each parameter.
        let names: Vec<&str> = children_by_field(params, "param")
            .into_iter()
            .filter_map(|p| p.child_by_field_name("name"))
            .map(|n| text_of(n, src))
            .collect();
        assert_eq!(names, vec!["name", "age"]);
    }

    #[test]
    fn walk_named_visits_pre_order_and_can_skip() {
        let src = "fn a() { return 1; }\nfn b() { return 2; }\n";
        let tree = parse(src);
        let mut kinds = Vec::new();
        walk_named(tree.root_node(), |node| {
            kinds.push(node.kind().to_string());
            // Skip descending into bodies, just to exercise skip semantics.
            node.kind() != "block"
        });
        assert_eq!(kinds.first().map(String::as_str), Some("module"));
        assert!(kinds.iter().filter(|k| *k == "fn_decl").count() == 2);
        // We skipped block, so no return_stmt should be in the list.
        assert!(!kinds.iter().any(|k| k == "return_stmt"));
    }

    #[test]
    fn text_of_invalid_range_returns_empty() {
        let src = "fn a() {}\n";
        let tree = parse(src);
        let module = tree.root_node();
        // Construct a sliced source string and ask for text_of with a range
        // outside it — we can't easily do that against the same node, but
        // we can call text_of with an unrelated source.
        assert_eq!(text_of(module, ""), "");
    }
}
