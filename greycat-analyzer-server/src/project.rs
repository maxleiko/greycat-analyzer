// use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};

// use crossbeam_channel::Sender;
// use greycat_analyzer_core::parse;
// use lsp_server::Message;
// use lsp_types::Uri;

// use crate::{Document, Result, publish_diagnostics};

// pub struct Project {
//     /// Path to this project's project.gcl file
//     path: PathBuf,
//     /// This projects modules
//     documents: HashMap<Uri, Rc<RefCell<Document>>>,
// }

// impl Project {
//     pub fn new(path: PathBuf) -> Self {
//         Self {
//             path,
//             documents: Default::default(),
//         }
//     }

//     pub fn add_module(&mut self, uri: Uri, document: Rc<RefCell<Document>>) {
//         self.documents.insert(uri, document);
//     }

//     pub fn remove_module(&mut self, uri: &Uri) -> Option<Rc<RefCell<Document>>> {
//         self.documents.remove(uri)
//     }

//     pub fn includes(&self, uri: &Uri) -> bool {
//         self.documents.contains_key(uri)
//     }

//     pub fn analyze(&self, sender: &Sender<Message>) -> Result<()> {
//         let mut diagnostics = Vec::new();
//         for (uri, doc) in self.documents.iter() {
//             let mut doc = doc.borrow_mut();
//             if doc.dirty {
//                 doc.dirty = false;
//                 diagnostics.clear();
//                 if let Err(err) = parse(doc.filename(), &doc.text, &mut diagnostics) {
//                     log::error!("unable to parse {} {err}", doc.uri.as_str());
//                 }
//                 publish_diagnostics(sender, uri.clone(), diagnostics.clone(), Some(doc.version))?;
//             }
//         }
//         Ok(())
//     }
// }

// impl std::fmt::Display for Project {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         write!(f, "Project({:?})", self.path)
//     }
// }
