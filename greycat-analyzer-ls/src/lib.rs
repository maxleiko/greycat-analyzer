#![allow(dead_code)] // TODO remove when stable

mod server;
mod backend;

pub use server::*;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
