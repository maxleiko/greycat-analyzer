use core::fmt;
use std::{
    cell::OnceCell,
    path::{Path, PathBuf},
};

use bumpalo::Bump;
use line_index::{LineCol, LineIndex};
use lsp_types::{TextDocumentContentChangeEvent, TextDocumentItem, Uri};

use crate::{
    cst::{self, CstStats, ParserCtx},
    tokenize,
};

#[derive(Debug)]
pub struct Document<'arena> {
    pub uri: Uri,
    pub version: i32,
    pub text: String,
    pub node: cst::Node<'arena>,
    filepath: OnceCell<PathBuf>,
}

impl<'arena> Document<'arena> {
    pub fn new(value: TextDocumentItem, arena: &'arena Bump) -> Self {
        let ctx = ParserCtx {
            arena,
            tokens: &tokenize(&value.text),
        };
        let node = cst::parse(ctx);

        Self {
            uri: value.uri,
            version: value.version,
            text: value.text,
            node,
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

    /// The module name
    pub fn name(&self) -> &str {
        let filename = self.filename();
        &filename[..filename.len() - 4]
    }

    pub(crate) fn apply_changes(
        &mut self,
        mut changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        arena: &'arena Bump,
    ) {
        if self.version >= version {
            return;
        }
        self.version = version;
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
            // we only deal with the Some variant, because None has been handled previously
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

        // update CST
        let tokens = tokenize(&self.text);
        self.node = cst::parse(ParserCtx {
            arena,
            tokens: &tokens,
        });
    }
}

impl fmt::Display for Document<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = CstStats::from(&self.node);

        write!(
            f,
            "{} [{}]{:x} [{stats}]",
            self.uri.as_str(),
            self.version,
            md5::compute(self.text.as_bytes()),
        )
    }
}
