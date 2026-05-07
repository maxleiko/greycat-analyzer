use core::fmt;
use std::{
    cell::OnceCell,
    path::{Path, PathBuf},
};

use line_index::{LineCol, LineIndex};
use lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Uri};
use tree_sitter::{InputEdit, Parser, Point, Tree};

use greycat_analyzer_syntax as syntax;

pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub tree: Tree,
    parser: Parser,
    filepath: OnceCell<PathBuf>,
}

impl Document {
    pub fn new(value: TextDocumentItem) -> Self {
        let mut parser = syntax::parser();
        let tree = parse(&mut parser, &value.text, None);
        Self {
            uri: value.uri,
            version: value.version,
            text: value.text,
            tree,
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
    ) {
        if self.version >= version {
            return;
        }
        self.version = version;

        // If any change is a full-document replace, reset text from the last
        // such change and process only the changes after it as incremental
        // edits. Mirrors rust-analyzer's behavior.
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

        let mut any_edit = false;
        for change in changes {
            let Some(range) = change.range else { continue };

            // Recompute LineIndex per change because earlier edits in the
            // same batch may have shifted the text.
            let index = LineIndex::new(&self.text);
            let Some(start_byte) = index.offset(LineCol {
                line: range.start.line,
                col: range.start.character,
            }) else {
                continue;
            };
            let Some(old_end_byte) = index.offset(LineCol {
                line: range.end.line,
                col: range.end.character,
            }) else {
                continue;
            };
            let start_byte = usize::from(start_byte);
            let old_end_byte = usize::from(old_end_byte);
            let new_end_byte = start_byte + change.text.len();

            let start_position = Point {
                row: range.start.line as usize,
                column: range.start.character as usize,
            };
            let old_end_position = Point {
                row: range.end.line as usize,
                column: range.end.character as usize,
            };

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
            // Incremental reparse, reusing the edited tree as a hint.
            let new_tree = parse(&mut self.parser, &self.text, Some(&self.tree));
            self.tree = new_tree;
        }
    }
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
        );
        assert_eq!(d.text, "fn final_text() {}\n");
    }
}
