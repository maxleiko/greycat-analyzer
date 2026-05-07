#![allow(dead_code)] // TODO remove when stable

mod backend;
pub mod capabilities;
mod server;

pub use server::*;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
