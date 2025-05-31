use std::path::PathBuf;

use dashmap::DashMap;
use tower_lsp::lsp_types::Url;

use crate::Document;

pub struct Project {
    /// Path to this project's project.gcl file
    path: PathBuf,
    /// This projects modules
    documents: DashMap<Url, Document>,
}

impl Project {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            documents: DashMap::new(),
        }
    }

    pub fn includes(&self, uri: &Url) -> bool {
        self.documents.contains_key(uri)
    }
}

impl std::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Project({:?})", self.path)
    }
}
