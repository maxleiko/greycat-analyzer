use std::ops::Range;

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
