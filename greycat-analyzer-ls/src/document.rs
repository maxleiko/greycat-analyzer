use std::{
    cell::OnceCell,
    path::{Path, PathBuf},
};

use line_index::{LineCol, LineIndex};
use lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Uri};

pub struct Document {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub dirty: bool,
    filepath: OnceCell<PathBuf>,
}

impl Document {
    pub fn filepath(&self) -> &Path {
        self.filepath
            .get_or_init(|| PathBuf::from(self.uri.path().as_str()))
    }

    pub fn filename(&self) -> &str {
        self.filepath().file_name().unwrap().to_str().unwrap()
    }

    pub fn apply_changes(
        &mut self,
        mut changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
    ) {
        if self.version >= version {
            return;
        }
        self.version = version;
        self.dirty = true;
        // if at least one of the changes is a full document change, use the last
        // of them as the starting point and ignore all previous changes
        // see: https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/lsp/utils.rs#L175
        let changes = match changes.iter().rposition(|change| change.range.is_none()) {
            Some(idx) => {
                self.text = std::mem::take(&mut changes[idx].text);
                &changes[idx + 1..]
            }
            None => &changes[..],
        };

        if changes.is_empty() {
            return;
        }

        let index = LineIndex::new(&self.text);
        for change in changes {
            // the None case can't happen as we handled it above already
            if let Some(range) = change.range {
                let start = index.offset(LineCol {
                    line: range.start.line,
                    col: range.start.character,
                });
                let end = index.offset(LineCol {
                    line: range.end.line,
                    col: range.end.character,
                });
                if let (Some(start), Some(end)) = (start, end) {
                    self.text
                        .replace_range(usize::from(start)..usize::from(end), &change.text);
                }
                // let start_char_idx = self.text.line_to_char(range.start.line as usize);
                // let start = start_char_idx + range.start.character as usize;
                // self.text.remove(start..start + len as usize);
                // self.text.insert(start, &change.text);
            }
        }
    }
}

impl From<TextDocumentItem> for Document {
    fn from(value: TextDocumentItem) -> Self {
        Self {
            uri: value.uri,
            version: value.version,
            text: value.text,
            filepath: OnceCell::new(),
            dirty: true,
        }
    }
}

impl std::fmt::Display for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "uri={}, version={}, len={}",
            self.uri.as_str(),
            self.version,
            self.text.len()
        )
    }
}
