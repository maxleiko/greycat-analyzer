use ropey::Rope;
use tower_lsp::lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Url};

pub struct Document {
    uri: Url,
    version: i32,
    text: Rope,
}

impl Document {
    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn apply_changes(&mut self, changes: Vec<TextDocumentContentChangeEvent>, version: i32) {
        self.version = version;
        for change in changes {
            self.apply_change(change);
        }
    }

    fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        let TextDocumentContentChangeEvent {
            range,
            range_length,
            text,
        } = change;
        match (range, range_length) {
            (None, None) => {
                // full text change
                self.text.remove(..);
                self.text.insert(0, &text);
            }
            (Some(range), Some(len)) => {
                // increment text change
                let start_char_idx = self.text.line_to_char(range.start.line as usize);
                let start = start_char_idx + range.start.character as usize;
                self.text.remove(start..start + len as usize);
                self.text.insert(start, &text);
            }
            (Some(_), None) | (None, Some(_)) => unreachable!(),
        }
    }

    // pub fn offset_at(&self, pos: Position) -> Option<usize> {
    //     let mut offset = 0;
    //     let mut lines = self.text.lines();

    //     let line = lines.nth(pos.line as usize)?;
    //     for (i, c) in line.line
    // }
}

impl From<TextDocumentItem> for Document {
    fn from(value: TextDocumentItem) -> Self {
        Self {
            uri: value.uri,
            version: value.version,
            text: Rope::from_str(&value.text),
        }
    }
}

impl std::fmt::Display for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "uri={}, version={}, bytes={}",
            self.uri,
            self.version,
            self.text.len_bytes()
        )
    }
}
