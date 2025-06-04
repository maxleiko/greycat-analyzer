use std::path::PathBuf;

use dashmap::DashMap;
use lsp_types::Uri;

use crate::Document;

pub struct Project {
    /// Path to this project's project.gcl file
    path: PathBuf,
    /// This projects modules
    documents: DashMap<Uri, Document>,
}

impl Project {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            documents: DashMap::new(),
        }
    }

    pub fn add_module(&mut self, uri: Uri, document: Document) {
        self.documents.insert(uri, document);
    }

    pub fn remove_module(&mut self, uri: &Uri) -> Option<(Uri, Document)> {
        self.documents.remove(uri)
    }

    pub fn includes(&self, uri: &Uri) -> bool {
        self.documents.contains_key(uri)
    }
}

impl std::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Project({:?})", self.path)
    }
}
