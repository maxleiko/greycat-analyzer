//! Shared fixtures for integration tests, replacing the single-file
//! `capabilities::*` shims with the project-aware path that the LSP
//! server actually dispatches to.
//!
//! Why: the legacy `capabilities::hover` / `references` / `rename` /
//! `completion` / `inlay_hints` / `code_actions` entry points re-run
//! `lower_module` + `resolve` + `analyze` from scratch on every call —
//! they bypass `ProjectAnalysis`, so a passing test against them is no
//! guarantee the LSP `textDocument/*` request works. The
//! `*_with_project` / `*_across_project` variants consume the cached
//! `ModuleAnalysis` (matching the server dispatch path); tests should
//! exercise *those*.

#![allow(dead_code)] // each test file imports a subset

use std::cell::Ref;
use std::path::Path;
use std::str::FromStr;

use greycat_analyzer_analysis::project::{ModuleAnalysis, ProjectAnalysis};
use greycat_analyzer_core::Document;
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_server::capabilities;
use lsp_types::{
    CodeActionOrCommand, CompletionList, GotoDefinitionResponse, Hover, InlayHint, Location,
    Position, WorkspaceEdit,
};

/// One-file project fixture — wraps a `SourceManager` populated with a
/// single `project` module plus its `ProjectAnalysis`, mirroring the
/// `Backend::project_for` state at the moment a request handler fires.
pub struct TestProject {
    pub manager: SourceManager,
    pub analysis: ProjectAnalysis,
    pub uri: Uri,
}

impl TestProject {
    /// Single-file project at `file:///proj/main.gcl` with lib
    /// `"project"`. Use this for tests that don't need multi-module
    /// shapes — most hover / references / rename / completion /
    /// inlay-hint cases.
    pub fn single_file(src: &str) -> Self {
        Self::single_file_at("/proj/main.gcl", src)
    }

    /// Single-file project at the given path. Use when the test cares
    /// about the URI's filename (e.g. workspace-symbol filtering).
    pub fn single_file_at(path: &str, src: &str) -> Self {
        let mut manager = SourceManager::new();
        let uri = Uri::from_str(&format!("file://{path}")).unwrap();
        manager.add_simple(uri.clone(), src, "project", false);
        let analysis = ProjectAnalysis::analyze(&manager);
        Self {
            manager,
            analysis,
            uri,
        }
    }

    /// Borrow the entry-point document. Mirrors what the LSP server's
    /// per-request handlers do via `cell.borrow()` before forwarding to
    /// the `*_with_project` capability.
    pub fn doc(&self) -> Ref<'_, Document> {
        self.manager
            .get(&self.uri)
            .expect("fixture's main URI must be in the manager")
            .borrow()
    }

    /// Cached `ModuleAnalysis` for the entry-point module — required
    /// by capabilities that consume it directly (`code_actions_with_project`,
    /// `inlay_hints_with_project`).
    pub fn module(&self) -> &ModuleAnalysis {
        self.analysis
            .module(&self.uri)
            .expect("fixture's main URI must have a ModuleAnalysis")
    }

    // -- Capability shortcuts ---------------------------------------------------
    //
    // Each method forwards to the project-aware `capabilities::*_with_project`
    // / `*_across_project` entry point with the fixture's cached state, so
    // tests exercise the same path the LSP server dispatches to.

    pub fn hover(&self, pos: Position) -> Option<Hover> {
        let doc = self.doc();
        capabilities::hover_with_project(
            &doc.text,
            &doc.lib,
            doc.root_node(),
            pos,
            &self.uri,
            &self.analysis,
            &self.manager,
        )
    }

    pub fn goto_definition(&self, pos: Position) -> Option<GotoDefinitionResponse> {
        capabilities::goto_definition_across_project(&self.analysis, &self.manager, &self.uri, pos)
    }

    pub fn references(&self, pos: Position) -> Vec<Location> {
        capabilities::references_across_project(&self.analysis, &self.manager, &self.uri, pos)
    }

    pub fn rename(&self, pos: Position, new_name: &str) -> Option<WorkspaceEdit> {
        capabilities::rename_across_project(&self.analysis, &self.manager, &self.uri, pos, new_name)
    }

    pub fn completion(&self, pos: Position) -> Option<CompletionList> {
        self.completion_at(pos, None)
    }

    pub fn completion_at(
        &self,
        pos: Position,
        project_root: Option<&Path>,
    ) -> Option<CompletionList> {
        let doc = self.doc();
        capabilities::completion_with_project(
            &doc.text,
            doc.root_node(),
            pos,
            &self.uri,
            &self.analysis,
            project_root,
        )
    }

    pub fn inlay_hints(&self, range: &lsp_types::Range) -> Vec<InlayHint> {
        let doc = self.doc();
        capabilities::inlay_hints_with_project(self.module(), &self.analysis, &doc.text, range)
    }

    pub fn code_actions(&self, range: lsp_types::Range) -> Vec<CodeActionOrCommand> {
        let doc = self.doc();
        capabilities::code_actions_with_project(self.module(), &doc.text, &self.uri, range)
    }
}

/// Resolve a cursor at the start of `needle` in `src`. Panics if
/// `needle` is absent — tests should match a unique substring to avoid
/// ambiguity. For positions inside the match use [`position_after`].
pub fn position_of(src: &str, needle: &str) -> Position {
    let off = src.find(needle).expect("needle present in src");
    byte_to_pos(src, off)
}

/// Resolve a cursor *after* `prefix` in `src` (e.g. for cursors that
/// must sit inside the parens of a call). The second arg is a docstring
/// hint of what follows; only used by readers, ignored at runtime.
pub fn position_after(src: &str, prefix: &str, _then: &str) -> Position {
    let off = src.find(prefix).expect("prefix present in src") + prefix.len();
    byte_to_pos(src, off)
}

fn byte_to_pos(src: &str, off: usize) -> Position {
    let line = src[..off].matches('\n').count() as u32;
    let col = (off - src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position {
        line,
        character: col,
    }
}
