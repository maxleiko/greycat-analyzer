//! LSP server for greycat — wires every analyzer / formatter
//! capability into a `lsp-server`-driven JSON-RPC loop.
//!
//! Two layers:
//!
//! - `backend` (private) — owns the
//!   [`greycat_analyzer_core::SourceManager`] +
//!   [`greycat_analyzer_analysis::project::ProjectAnalysis`] cache,
//!   dispatches `did_open` / `did_change` / `did_save` /
//!   `did_close` notifications, and publishes diagnostics on every
//!   change.
//! - [`capabilities`] — per-request handlers (hover, goto-def,
//!   references, rename, formatting, etc.). Each function takes raw
//!   text + parsed tree + cursor and returns the LSP response shape
//!   directly so the same code is callable from the CLI / wasm /
//!   integration tests without going through JSON-RPC.
//!
//! The transport / event loop lives in `server`, started via
//! [`start_server`]. The crate is shipped as a library so the
//! `greycat-analyzer` binary picks up the `server` subcommand by
//! linking against this crate.

#![allow(dead_code)] // TODO remove when stable

mod backend;
pub mod capabilities;
pub(crate) mod conv;
pub mod registry;
mod server;
mod watcher;

pub use server::*;

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
