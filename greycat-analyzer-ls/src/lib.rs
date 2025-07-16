#![allow(dead_code)] // TODO remove when stable

mod document;
mod lang_server;
mod project;

pub use document::*;
pub use lang_server::*;
pub use project::*;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
