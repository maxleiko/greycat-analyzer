use std::{
    cell::{Ref, RefCell},
    collections::HashMap,
};

use lsp_types::{TextDocumentContentChangeEvent, Uri};

use crate::Document;

#[derive(Debug, Default)]
pub struct Manager {
    documents: HashMap<Uri, RefCell<Document>>,
}

impl Manager {
    pub fn add(&mut self, doc: Document) {
        self.documents.insert(doc.uri.clone(), RefCell::new(doc));
    }

    pub fn get(&self, uri: &Uri) -> Option<&RefCell<Document>> {
        self.documents.get(uri)
    }

    pub fn update(
        &mut self,
        uri: &Uri,
        changes: Vec<TextDocumentContentChangeEvent>,
        version: i32,
    ) -> Ref<'_, Document> {
        if let Some(doc) = self.documents.get(uri) {
            doc.borrow_mut().apply_changes(changes, version);
            doc.borrow()
        } else {
            panic!("cannot update unknown document")
        }
    }
}

impl std::fmt::Display for Manager {
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
