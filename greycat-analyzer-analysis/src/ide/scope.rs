//! Shared "what names are visible at this byte" walker.
//!
//! Extracted from [`super::completion`] so non-completion capabilities
//! (currently [`super::quickfix`]'s `unused-local` rename) can reuse
//! the same scope semantics without growing a parallel implementation.
//!
//! The walker is HIR-based, not CST-based: it descends from the
//! module's `Decl` list into the function / type body that contains
//! the cursor, mirroring the resolver's lexical scoping (parameters
//! before locals, pre-cursor `var` bindings, for-init / for-in binders
//! while the cursor is inside the loop body, catch parameters while
//! inside the catch block). It does **not** consult `Resolutions` —
//! the scope structure is recoverable from the HIR alone.

use greycat_analyzer_core::{Symbol, SymbolTable};
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::hir::{BlockStmt, Decl, Ident, Stmt};
use rustc_hash::FxHashSet;

use crate::conv::stmt_byte_range;

/// Where a [`scope_names_at`] entry came from. Lets callers reach back
/// to the underlying decl / binding for follow-up queries
/// (completion's signature / type rendering, etc.).
#[derive(Debug, Clone, Copy)]
pub enum NameSource {
    /// Top-level decl in the current module (`fn` / `type` / `enum` /
    /// `var`).
    ModuleDecl(Idx<Decl>),
    /// Local `var x = …` binding. Carries the *binding* name idx so
    /// downstream `def_types` lookups resolve the inferred type.
    Local(Idx<Ident>),
    /// Function parameter. Same payload as `Local` — callers
    /// disambiguate via the variant.
    Param(Idx<Ident>),
    /// Generic type parameter (`fn<T>` / `type Foo<T>`).
    Generic,
}

/// Kind of a scope entry, decoupled from any LSP shape. Callers map
/// to their own item-kind enum (e.g. `lsp_types::CompletionItemKind`)
/// at their own boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeNameKind {
    Fn,
    Type,
    Enum,
    Var,
    Local,
    Param,
    Generic,
}

/// One name visible at a given byte position.
#[derive(Debug, Clone)]
pub struct ScopeName {
    pub symbol: Symbol,
    pub kind: ScopeNameKind,
    pub source: NameSource,
}

/// Tight convenience for callers that only need "is this name taken
/// at this byte" — drops the kind / source metadata. Used by
/// `quickfix::unused_local_fix` for collision-checking a candidate
/// rename target.
pub fn names_in_scope_at(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    byte: usize,
) -> FxHashSet<Symbol> {
    let mut out = FxHashSet::default();
    walk(hir, symbols, byte, &mut |entry| {
        out.insert(entry.symbol);
    });
    out
}

/// Full walker — invokes `emit` for each visible name. Use this when
/// you need the kind / source metadata; otherwise prefer
/// [`names_in_scope_at`].
pub fn scope_names_at(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    byte: usize,
) -> Vec<ScopeName> {
    let mut out = Vec::new();
    walk(hir, symbols, byte, &mut |entry| out.push(entry));
    out
}

fn walk(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    cursor_byte: usize,
    emit: &mut dyn FnMut(ScopeName),
) {
    let Some(module) = hir.module.as_ref() else {
        return;
    };
    // Module-level decls are always visible (forward-ref allowed).
    for &decl_id in &module.decls {
        if let Some(name_id) = hir.decls[decl_id].name() {
            let symbol = hir.idents[name_id].symbol;
            let kind = match &hir.decls[decl_id] {
                Decl::Fn(_) => ScopeNameKind::Fn,
                Decl::Type(_) => ScopeNameKind::Type,
                Decl::Enum(_) => ScopeNameKind::Enum,
                Decl::Var(_) => ScopeNameKind::Var,
                Decl::Pragma(_) => continue,
            };
            emit(ScopeName {
                symbol,
                kind,
                source: NameSource::ModuleDecl(decl_id),
            });
        }
    }
    // Descend into the declaration that contains the cursor.
    for &decl_id in &module.decls {
        let r = hir.decls[decl_id].byte_range();
        if !(r.start <= cursor_byte && cursor_byte <= r.end) {
            continue;
        }
        match &hir.decls[decl_id] {
            Decl::Fn(d) => collect_fn_scope(hir, symbols, d, cursor_byte, emit),
            Decl::Type(d) => {
                for g in &d.generics {
                    let symbol = hir.idents[*g].symbol;
                    emit(ScopeName {
                        symbol,
                        kind: ScopeNameKind::Generic,
                        source: NameSource::Generic,
                    });
                }
                for &m_id in &d.methods {
                    let mr = hir.decls[m_id].byte_range();
                    if !(mr.start <= cursor_byte && cursor_byte <= mr.end) {
                        continue;
                    }
                    if let Decl::Fn(fd) = &hir.decls[m_id] {
                        collect_fn_scope(hir, symbols, fd, cursor_byte, emit);
                    }
                }
            }
            _ => {}
        }
    }
    let _ = symbols;
}

fn collect_fn_scope(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    fnd: &greycat_analyzer_hir::hir::FnDecl,
    cursor_byte: usize,
    emit: &mut dyn FnMut(ScopeName),
) {
    for g in &fnd.generics {
        let symbol = hir.idents[*g].symbol;
        emit(ScopeName {
            symbol,
            kind: ScopeNameKind::Generic,
            source: NameSource::Generic,
        });
    }
    for p in &fnd.params {
        let p = &hir.fn_params[*p];
        let symbol = hir.idents[p.name].symbol;
        emit(ScopeName {
            symbol,
            kind: ScopeNameKind::Param,
            source: NameSource::Param(p.name),
        });
    }
    if let Some(body) = fnd.body {
        collect_stmt_scope(hir, symbols, body, cursor_byte, emit);
    }
}

fn cursor_in_block(block: &BlockStmt, cursor_byte: usize) -> bool {
    block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end
}

/// Walk a `BlockStmt` collecting cursor-visible names. Pre-cursor
/// `var` bindings surface; in-cursor stmts recurse. Mirrors the
/// resolver's per-block scope.
fn collect_block_scope(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    block: &BlockStmt,
    cursor_byte: usize,
    emit: &mut dyn FnMut(ScopeName),
) {
    if !(block.byte_range.start <= cursor_byte && cursor_byte <= block.byte_range.end) {
        return;
    }
    for s in &block.stmts {
        let r = stmt_byte_range(hir, *s);
        if r.end <= cursor_byte {
            if let Stmt::Var(lv) = &hir.stmts[*s] {
                let symbol = hir.idents[lv.name].symbol;
                emit(ScopeName {
                    symbol,
                    kind: ScopeNameKind::Local,
                    source: NameSource::Local(lv.name),
                });
            }
        } else if r.start <= cursor_byte && cursor_byte <= r.end {
            collect_stmt_scope(hir, symbols, *s, cursor_byte, emit);
        }
    }
}

fn collect_stmt_scope(
    hir: &greycat_analyzer_hir::Hir,
    symbols: &SymbolTable,
    stmt_id: Idx<Stmt>,
    cursor_byte: usize,
    emit: &mut dyn FnMut(ScopeName),
) {
    match &hir.stmts[stmt_id] {
        Stmt::Block(b) => collect_block_scope(hir, symbols, b, cursor_byte, emit),
        Stmt::If(s) => {
            collect_block_scope(hir, symbols, &s.then_branch, cursor_byte, emit);
            if let Some(eb) = s.else_branch {
                let er = stmt_byte_range(hir, eb);
                if er.start <= cursor_byte && cursor_byte <= er.end {
                    collect_stmt_scope(hir, symbols, eb, cursor_byte, emit);
                }
            }
        }
        Stmt::While(s) => collect_block_scope(hir, symbols, &s.body, cursor_byte, emit),
        Stmt::DoWhile(s) => collect_block_scope(hir, symbols, &s.body, cursor_byte, emit),
        Stmt::For(s) if cursor_in_block(&s.body, cursor_byte) => {
            if let Some(name_id) = s.init_name {
                let symbol = hir.idents[name_id].symbol;
                emit(ScopeName {
                    symbol,
                    kind: ScopeNameKind::Local,
                    source: NameSource::Local(name_id),
                });
            }
            collect_block_scope(hir, symbols, &s.body, cursor_byte, emit);
        }
        Stmt::ForIn(s) if cursor_in_block(&s.body, cursor_byte) => {
            for p in &s.params {
                let symbol = hir.idents[p.name].symbol;
                emit(ScopeName {
                    symbol,
                    kind: ScopeNameKind::Local,
                    source: NameSource::Local(p.name),
                });
            }
            collect_block_scope(hir, symbols, &s.body, cursor_byte, emit);
        }
        Stmt::Try(s) => {
            collect_block_scope(hir, symbols, &s.try_block, cursor_byte, emit);
            if cursor_in_block(&s.catch_block, cursor_byte) {
                if let Some(err_id) = s.error_param {
                    let symbol = hir.idents[err_id].symbol;
                    emit(ScopeName {
                        symbol,
                        kind: ScopeNameKind::Local,
                        source: NameSource::Local(err_id),
                    });
                }
                collect_block_scope(hir, symbols, &s.catch_block, cursor_byte, emit);
            }
        }
        Stmt::At(s) => collect_block_scope(hir, symbols, &s.block, cursor_byte, emit),
        _ => {}
    }
}
