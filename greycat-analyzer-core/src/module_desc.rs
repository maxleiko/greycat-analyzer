//! `mod_pragma` extraction — the small CST traversal that pulls
//! `@library("...")` and `@include("...")` declarations out of a parsed
//! module so the source manager can drive recursive loads.
//!
//! Ports `packages/lang/src/project/module_desc.ts` (~147 LoC) over
//! tree-sitter instead of the legacy CST. The shape is faithful: a
//! [`ModuleDesc`] holds spanned `libraries`, `includes`, and `others`
//! (every other top-level pragma name → its annotation node).

use std::ops::Range;

use lsp_types::Uri;

use greycat_analyzer_syntax::tree_sitter;

/// Spanned slice of source text — start/end byte offsets plus the resolved
/// string. Kept stringly-typed for now; once the analyzer's full Span/Diag
/// types land in P1.4, this will move onto those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub byte_range: Range<usize>,
}

/// `@library("name", "version")` declaration. `version` may be empty if
/// the second arg is missing or non-stringly — same tolerant behavior as
/// TS `parseModuleDesc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRef {
    pub uri: Uri,
    pub name: String,
    pub version: Option<String>,
    pub byte_range: Range<usize>,
}

/// `@include("path")` declaration.
pub type IncludeRef = Spanned<String>;

/// Result of [`parse_module_desc`].
#[derive(Debug, Default, Clone)]
pub struct ModuleDesc {
    pub libraries: Vec<LibraryRef>,
    pub includes: Vec<IncludeRef>,
    /// Every other top-level annotation name (e.g. `expose`, `permission`,
    /// `role`) mapped to its `byte_range`. Useful for downstream phases
    /// that care about pragma presence without re-walking the tree.
    pub others: Vec<Spanned<String>>,
}

/// Walk the `mod_pragma` children of `root` (which must be a `module`
/// node) and bucket each into [`ModuleDesc`]. Mirrors TS
/// `parseModuleDesc`: only string-literal arguments to `@library` and
/// `@include` are accepted; malformed declarations are silently dropped.
pub fn parse_module_desc(uri: Uri, source: &str, root: tree_sitter::Node<'_>) -> ModuleDesc {
    let mut desc = ModuleDesc::default();
    if root.kind() != "module" {
        return desc;
    }

    let mut cursor = root.walk();
    for stmt in root.named_children(&mut cursor) {
        if stmt.kind() != "mod_pragma" {
            continue;
        }
        // mod_pragma children: optional doc, annotation, _semi (anonymous).
        // Find the annotation.
        let Some(annotation) = stmt
            .named_children(&mut stmt.walk())
            .find(|c| c.kind() == "annotation")
        else {
            continue;
        };

        // annotation children: ident, optional args.
        let mut ann_cursor = annotation.walk();
        let mut name_node: Option<tree_sitter::Node<'_>> = None;
        let mut args_node: Option<tree_sitter::Node<'_>> = None;
        for c in annotation.named_children(&mut ann_cursor) {
            match c.kind() {
                "ident" if name_node.is_none() => name_node = Some(c),
                "args" => args_node = Some(c),
                _ => {}
            }
        }
        let Some(name_node) = name_node else { continue };
        let name = node_text(source, name_node).to_string();
        let span = stmt.byte_range();

        match name.as_str() {
            "library" => {
                if let Some(args) = args_node {
                    let mut s_args = string_args(source, args);
                    let lib_name = s_args.next();
                    let version = s_args.next();
                    if let Some(lib_name) = lib_name {
                        desc.libraries.push(LibraryRef {
                            uri: uri.clone(),
                            name: lib_name,
                            version,
                            byte_range: span,
                        });
                    }
                }
            }
            "include" => {
                if let Some(args) = args_node
                    && let Some(path) = string_args(source, args).next()
                {
                    desc.includes.push(Spanned {
                        value: path,
                        byte_range: span,
                    });
                }
            }
            _ => {
                desc.others.push(Spanned {
                    value: name,
                    byte_range: span,
                });
            }
        }
    }

    desc
}

fn node_text<'src>(source: &'src str, node: tree_sitter::Node<'_>) -> &'src str {
    let r = node.byte_range();
    source.get(r).unwrap_or("")
}

/// Extract the raw text of every string-literal child in `args`. Tree-sitter
/// `string` nodes wrap one or more `string_fragment` children — we
/// concatenate the fragments to get the unescaped logical string. (Escapes
/// like `\n` will land as `string_escape_sequence` siblings; we ignore those
/// for module_desc purposes — `@library` / `@include` arguments are simple
/// identifiers in practice.)
fn string_args<'src, 'tree>(
    source: &'src str,
    args: tree_sitter::Node<'tree>,
) -> impl Iterator<Item = String> + use<'src, 'tree> {
    let mut cursor = args.walk();
    let children: Vec<_> = args
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "string")
        .collect();

    children.into_iter().map(move |s| {
        let mut acc = String::new();
        let mut sc = s.walk();
        for piece in s.named_children(&mut sc) {
            if piece.kind() == "string_fragment" {
                acc.push_str(node_text(source, piece));
            }
        }
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn fixture(source: &str) -> ModuleDesc {
        let tree = greycat_analyzer_syntax::parse(source);
        let uri = Uri::from_str("file:///mod.gcl").unwrap();
        parse_module_desc(uri, source, tree.root_node())
    }

    #[test]
    fn extracts_library_declarations() {
        let desc = fixture("@library(\"std\", \"1.2.3\");\n");
        assert_eq!(desc.libraries.len(), 1);
        assert_eq!(desc.libraries[0].name, "std");
        assert_eq!(desc.libraries[0].version.as_deref(), Some("1.2.3"));
        assert!(desc.includes.is_empty());
        assert!(desc.others.is_empty());
    }

    #[test]
    fn library_without_version_is_still_captured() {
        let desc = fixture("@library(\"std\");\n");
        assert_eq!(desc.libraries.len(), 1);
        assert_eq!(desc.libraries[0].name, "std");
        assert_eq!(desc.libraries[0].version, None);
    }

    #[test]
    fn extracts_include_declarations() {
        let desc = fixture("@include(\"src\");\n@include(\"vendor/foo\");\n");
        assert_eq!(desc.includes.len(), 2);
        assert_eq!(desc.includes[0].value, "src");
        assert_eq!(desc.includes[1].value, "vendor/foo");
    }

    #[test]
    fn other_pragmas_bucketed() {
        let desc = fixture("@expose(\"some\");\n@role(\"admin\");\n");
        assert_eq!(desc.libraries.len(), 0);
        assert_eq!(desc.includes.len(), 0);
        assert_eq!(desc.others.len(), 2);
        assert_eq!(desc.others[0].value, "expose");
        assert_eq!(desc.others[1].value, "role");
    }

    #[test]
    fn malformed_library_silently_dropped() {
        // Library with no string args — TS port silently drops.
        let desc = fixture("@library;\n");
        assert!(desc.libraries.is_empty());
    }

    #[test]
    fn doc_comment_does_not_confuse_walker() {
        let desc = fixture("/// docs\n@library(\"std\", \"1.0\");\n");
        assert_eq!(desc.libraries.len(), 1);
        assert_eq!(desc.libraries[0].name, "std");
    }
}
