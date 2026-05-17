use core::fmt;
use std::{
    cell::OnceCell,
    path::{Path, PathBuf},
};

use line_index::{LineCol, LineIndex, WideEncoding, WideLineCol};
use lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Uri};
use tree_sitter::{InputEdit, Parser, Point, Tree};

use greycat_analyzer_syntax as syntax;

use crate::SourceEncoding;

pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub tree: Tree,
    /// The library this document belongs to. `"project"` for project-local
    /// modules, the library name (e.g. `"std"`) for files loaded via
    /// `@library(...)`. Mirrors TS `SourceFile.lib`.
    pub lib: String,
    /// `true` if the document was opened by the LSP client (vs.
    /// transitively loaded from disk by the source manager). Mirrors
    /// TS `SourceFile.opened`.
    pub opened: bool,
    parser: Parser,
    filepath: OnceCell<PathBuf>,
}

impl Document {
    /// Create a document for an LSP-opened text document. `lib` defaults
    /// to `"project"`.
    pub fn new(value: TextDocumentItem) -> Self {
        Self::with_lib(value, "project", true)
    }

    pub fn with_lib(value: TextDocumentItem, lib: impl Into<String>, opened: bool) -> Self {
        let mut parser = syntax::parser();
        let tree = parse(&mut parser, &value.text, None);
        Self {
            uri: value.uri,
            version: value.version,
            text: value.text,
            tree,
            lib: lib.into(),
            opened,
            parser,
            filepath: OnceCell::new(),
        }
    }

    /// The absolute path
    pub fn filepath(&self) -> &Path {
        self.filepath
            .get_or_init(|| PathBuf::from(self.uri.path().as_str()))
    }

    /// The filename with the extension
    pub fn filename(&self) -> &str {
        self.filepath().file_name().unwrap().to_str().unwrap()
    }

    /// The module name (filename without `.gcl` extension)
    pub fn name(&self) -> &str {
        let filename = self.filename();
        &filename[..filename.len() - 4]
    }

    /// Convenience accessor for the parsed tree's root node.
    pub fn root_node(&self) -> tree_sitter::Node<'_> {
        self.tree.root_node()
    }

    pub(crate) fn apply_changes(
        &mut self,
        mut changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        encoding: SourceEncoding,
    ) {
        if self.version >= version {
            return;
        }
        self.version = version;

        let changes = match changes.iter().rposition(|c| c.range.is_none()) {
            Some(idx) => {
                self.text = std::mem::take(&mut changes[idx].text);
                self.tree = parse(&mut self.parser, &self.text, None);
                &changes[idx + 1..]
            }
            None => &changes[..],
        };

        if changes.is_empty() {
            return;
        }

        // Tree-sitter's incremental parse contract is "share structure
        // with subtrees the edit didn't touch" — NOT "produce the same
        // tree a fresh parse would." When the pre-edit tree carries an
        // `ERROR` subtree from a prior recovery, the next incremental
        // parse keeps that subtree's byte range as "unedited" and
        // reuses it, even when the new text inside it is now
        // syntactically valid. The result is stale `ERROR` nodes that
        // persist across edits and can't be cleared without a fresh
        // parse. Capture the pre-edit error state here; when it was
        // dirty, skip the incremental path below and re-parse from
        // scratch after applying the text mutations.
        let old_tree_had_errors = self.tree.root_node().has_error();

        let mut any_edit = false;
        for change in changes {
            let Some(range) = change.range else { continue };

            let index = LineIndex::new(&self.text);

            let Some(start_byte) =
                lsp_pos_to_offset(&index, range.start.line, range.start.character, encoding)
            else {
                continue;
            };
            let Some(old_end_byte) =
                lsp_pos_to_offset(&index, range.end.line, range.end.character, encoding)
            else {
                continue;
            };

            // Guard against landing mid-codepoint (defensive; to_utf8 should
            // always return char boundaries, but replace_range panics if not).
            if !self.text.is_char_boundary(start_byte) || !self.text.is_char_boundary(old_end_byte)
            {
                continue;
            }

            // Tree-sitter Points use byte-offset-within-row for `column`.
            let start_line_byte =
                lsp_pos_to_offset(&index, range.start.line, 0, encoding).unwrap_or(start_byte);
            let end_line_byte =
                lsp_pos_to_offset(&index, range.end.line, 0, encoding).unwrap_or(old_end_byte);

            let start_position = Point {
                row: range.start.line as usize,
                column: start_byte - start_line_byte,
            };
            let old_end_position = Point {
                row: range.end.line as usize,
                column: old_end_byte - end_line_byte,
            };

            let new_end_byte = start_byte + change.text.len();
            self.text
                .replace_range(start_byte..old_end_byte, &change.text);

            let new_end_position = advance_point(start_position, &change.text);

            self.tree.edit(&InputEdit {
                start_byte,
                old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
            any_edit = true;
        }

        if any_edit {
            // Incremental reuse only when the pre-edit tree was clean
            // (see the comment above `old_tree_had_errors`). When the
            // old tree was dirty, drop it and re-parse from scratch
            // so any committed-from-recovery `ERROR` subtree doesn't
            // get carried forward.
            let old_tree = if old_tree_had_errors {
                None
            } else {
                Some(&self.tree)
            };
            let new_tree = parse(&mut self.parser, &self.text, old_tree);
            self.tree = new_tree;
        }
    }
}

fn lsp_pos_to_offset(
    index: &LineIndex,
    line: u32,
    character: u32,
    encoding: SourceEncoding,
) -> Option<usize> {
    let offset = match encoding {
        SourceEncoding::UTF8 => {
            // LSP col is already a UTF-8 byte offset within the line.
            index.offset(LineCol {
                line,
                col: character,
            })?
        }
        SourceEncoding::UTF16 => {
            // LSP col is a UTF-16 code-unit offset; convert via to_utf8 first.
            let wide = WideLineCol {
                line,
                col: character,
            };
            let line_col = index.to_utf8(WideEncoding::Utf16, wide)?;
            index.offset(line_col)?
        }
    };
    Some(usize::from(offset))
}

fn parse(parser: &mut Parser, text: &str, old_tree: Option<&Tree>) -> Tree {
    parser
        .parse(text, old_tree)
        .expect("tree-sitter parse never returns None without a cancellation flag")
}

/// Advance a `Point` by walking `inserted`, treating `\n` as a line break.
/// Column is tracked in bytes — consistent with how byte offsets are computed
/// from LSP positions elsewhere in this module.
fn advance_point(start: Point, inserted: &str) -> Point {
    let mut row = start.row;
    let mut column = start.column;
    for ch in inserted.chars() {
        if ch == '\n' {
            row += 1;
            column = 0;
        } else {
            column += ch.len_utf8();
        }
    }
    Point { row, column }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("uri", &self.uri.as_str())
            .field("version", &self.version)
            .field("text_len", &self.text.len())
            .field("tree", &self.tree)
            .finish()
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}]{:x} ({} bytes)",
            self.uri.as_str(),
            self.version,
            md5::compute(self.text.as_bytes()),
            self.text.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range, TextDocumentContentChangeEvent, TextDocumentItem, Uri};
    use std::str::FromStr;

    fn doc(text: &str) -> Document {
        Document::new(TextDocumentItem {
            uri: Uri::from_str("file:///mod.gcl").unwrap(),
            language_id: "greycat".into(),
            version: 1,
            text: text.into(),
        })
    }

    #[test]
    fn new_parses_to_module_root() {
        let d = doc("fn main() {}\n");
        assert_eq!(d.root_node().kind(), "module");
        assert!(!d.root_node().has_error());
    }

    #[test]
    fn full_replace_change_swaps_text_and_tree() {
        let mut d = doc("fn a() {}\n");
        let new_text = "fn b() { return 1; }\n".to_string();
        d.apply_changes(
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: new_text.clone(),
            }],
            2,
            SourceEncoding::UTF8,
        );
        assert_eq!(d.text, new_text);
        assert_eq!(d.version, 2);
        // Source text should now contain `return`.
        let root = d.root_node();
        assert!(!root.has_error());
        assert!(d.text.contains("return"));
    }

    #[test]
    fn incremental_range_change_updates_tree() {
        let mut d = doc("fn main() {}\n");
        // Replace `main` with `foo`: line 0, characters 3..7.
        d.apply_changes(
            vec![TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 7,
                    },
                }),
                range_length: None,
                text: "foo".into(),
            }],
            2,
            SourceEncoding::UTF8,
        );
        assert_eq!(d.text, "fn foo() {}\n");
        let root = d.root_node();
        assert_eq!(root.kind(), "module");
        assert!(!root.has_error());
    }

    #[test]
    fn out_of_order_version_is_ignored() {
        let mut d = doc("fn a() {}\n");
        d.apply_changes(
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "fn b() {}\n".into(),
            }],
            0, // older than current version 1
            SourceEncoding::UTF8,
        );
        assert_eq!(d.text, "fn a() {}\n");
        assert_eq!(d.version, 1);
    }

    #[test]
    fn batch_with_full_replace_uses_last_full() {
        let mut d = doc("fn a() {}\n");
        d.apply_changes(
            vec![
                TextDocumentContentChangeEvent {
                    range: Some(Range {
                        start: Position {
                            line: 0,
                            character: 3,
                        },
                        end: Position {
                            line: 0,
                            character: 4,
                        },
                    }),
                    range_length: None,
                    text: "z".into(),
                },
                TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "fn final_text() {}\n".into(),
                },
            ],
            2,
            SourceEncoding::UTF8,
        );
        assert_eq!(d.text, "fn final_text() {}\n");
    }
}
