use std::{
    cell::{Ref, RefCell},
    collections::HashMap,
};

use bumpalo::Bump;
use lsp_types::{TextDocumentContentChangeEvent, Uri};

use crate::Document;

#[derive(Debug, Default)]
pub struct Manager<'arena> {
    documents: HashMap<Uri, RefCell<Document<'arena>>>,
}

impl<'arena> Manager<'arena> {
    pub fn add(&mut self, doc: Document<'arena>) {
        self.documents.insert(doc.uri.clone(), RefCell::new(doc));
    }

    pub fn get(&self, uri: &Uri) -> Option<&RefCell<Document<'arena>>> {
        self.documents.get(uri)
    }

    pub fn update(
        &mut self,
        uri: &Uri,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
        arena: &'arena Bump,
    ) -> Ref<'_, Document<'arena>> {
        if let Some(doc) = self.documents.get(uri) {
            doc.borrow_mut().apply_changes(changes, version, arena);
            doc.borrow()
        } else {
            panic!("cannot update unknown document")
        }
    }
}

impl std::fmt::Display for Manager<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Manager({}):", self.documents.len())?;
        let last_i = self.documents.len().saturating_sub(1);
        for (i, doc) in self.documents.values().enumerate() {
            let doc = doc.borrow();
            write!(f, "{doc}")?;
            if i < last_i {
                writeln!(f)?;
            }
        }
        Ok(())
    }
}
