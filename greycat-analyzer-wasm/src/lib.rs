//! WASM API surface for the greycat analyzer (P5.1).
//!
//! Every analyzer stage gets its own export so the playground can render
//! them side-by-side. Each function takes a `&str` source plus optional
//! configuration and returns a `JsValue` — typically a JSON-serializable
//! struct so the TS playground can walk it as a normal object.
//!
//! Stages exposed:
//! - [`parse_sexp`]: tree-sitter s-expression (existing API kept for
//!   back-compat).
//! - [`parse_tree`]: serialized CST as nested objects with kind / range /
//!   field / children.
//! - [`tokens`]: a flat list of named-node "tokens" (kind + range + text)
//!   for the syntax-highlight / tokens panel.
//! - [`lower_hir`]: serialized HIR module + arenas.
//! - [`infer_types`]: per-expression inferred types (display strings).
//! - [`diagnostics`]: parse + semantic + lint diagnostics, all merged.
//! - [`format`]: formatted source.
//!
//! Decision: each export does its own pipeline run. Caching across
//! exports requires an opaque session handle, which is overkill for an
//! interactive playground (re-running the pipeline on every keystroke
//! is microseconds for typical sources). Add caching when profiling
//! shows the playground is bottlenecked here.

use serde::Serialize;
use wasm_bindgen::prelude::*;

use greycat_analyzer_analysis::{
    analyzer::{Severity, analyze},
    lint::{LintSeverity, run_lints},
    resolver::resolve,
};
use greycat_analyzer_core::diagnostics::parse_diagnostics;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::lower_module;
use greycat_analyzer_syntax::tree_sitter;

// =============================================================================
// Helpers
// =============================================================================

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[derive(Serialize)]
struct ByteRange {
    start: usize,
    end: usize,
}

impl From<std::ops::Range<usize>> for ByteRange {
    fn from(r: std::ops::Range<usize>) -> Self {
        Self {
            start: r.start,
            end: r.end,
        }
    }
}

#[derive(Serialize)]
struct Position {
    line: u32,
    column: u32,
}

fn pos_at(text: &str, byte: usize) -> Position {
    let mut line = 0u32;
    let mut column = 0u32;
    let prefix = &text[..byte.min(text.len())];
    for c in prefix.chars() {
        if c == '\n' {
            line += 1;
            column = 0;
        } else {
            column += c.len_utf8() as u32;
        }
    }
    Position { line, column }
}

// =============================================================================
// parse_sexp
// =============================================================================

#[wasm_bindgen]
pub fn parse_sexp(source: &str) -> String {
    greycat_analyzer_syntax::parse(source)
        .root_node()
        .to_sexp()
}

// =============================================================================
// parse_tree
// =============================================================================

#[derive(Serialize)]
struct CstNode {
    kind: String,
    field: Option<String>,
    is_named: bool,
    is_error: bool,
    is_missing: bool,
    range: ByteRange,
    text: Option<String>,
    children: Vec<CstNode>,
}

fn cst_to_json(node: tree_sitter::Node<'_>, source: &str, field: Option<&str>) -> CstNode {
    let mut children = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let cf = cursor.field_name().map(str::to_owned);
            children.push(cst_to_json(child, source, cf.as_deref()));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    let is_leaf = children.is_empty();
    CstNode {
        kind: node.kind().to_string(),
        field: field.map(str::to_owned),
        is_named: node.is_named(),
        is_error: node.is_error(),
        is_missing: node.is_missing(),
        range: node.byte_range().into(),
        text: if is_leaf {
            source.get(node.byte_range()).map(str::to_owned)
        } else {
            None
        },
        children,
    }
}

#[wasm_bindgen]
pub fn parse_tree(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let root = cst_to_json(tree.root_node(), source, None);
    to_js(&root)
}

// =============================================================================
// tokens
// =============================================================================

#[derive(Serialize)]
struct Token {
    kind: String,
    range: ByteRange,
    start: Position,
    end: Position,
    text: String,
}

#[wasm_bindgen]
pub fn tokens(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let mut out = Vec::new();
    fn walk<'a>(
        node: tree_sitter::Node<'a>,
        source: &str,
        out: &mut Vec<Token>,
    ) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                if !child.is_named() && !is_significant_anon(child.kind()) {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                }
                walk(child, source, out);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        if node.child_count() == 0 {
            let r = node.byte_range();
            out.push(Token {
                kind: node.kind().to_string(),
                range: r.clone().into(),
                start: pos_at(source, r.start),
                end: pos_at(source, r.end),
                text: source.get(r).unwrap_or("").to_string(),
            });
        }
    }
    walk(tree.root_node(), source, &mut out);
    to_js(&out)
}

fn is_significant_anon(kind: &str) -> bool {
    // Punctuation and keywords are still useful for the tokens view; we
    // emit them. Whitespace-shaped trivia gets dropped by tree-sitter.
    !kind.is_empty()
}

// =============================================================================
// lower_hir
// =============================================================================

#[derive(Serialize)]
struct HirSummary {
    module_name: String,
    lib: String,
    decls: Vec<HirDecl>,
    counts: HirCounts,
}

#[derive(Serialize)]
struct HirDecl {
    kind: &'static str,
    name: String,
    range: ByteRange,
}

#[derive(Serialize)]
struct HirCounts {
    decls: usize,
    stmts: usize,
    exprs: usize,
    type_refs: usize,
    idents: usize,
}

fn summarize_hir(hir: &Hir) -> HirSummary {
    let module = hir.module.clone().unwrap_or_else(|| {
        greycat_analyzer_hir::types::Module {
            name: "<empty>".into(),
            lib: "project".into(),
            decls: Vec::new(),
            byte_range: 0..0,
        }
    });
    let decls = module
        .decls
        .iter()
        .map(|d_id| {
            let d = &hir.decls[*d_id];
            let kind = match d {
                greycat_analyzer_hir::types::Decl::Fn(_) => "fn",
                greycat_analyzer_hir::types::Decl::Type(_) => "type",
                greycat_analyzer_hir::types::Decl::Enum(_) => "enum",
                greycat_analyzer_hir::types::Decl::Var(_) => "var",
                greycat_analyzer_hir::types::Decl::Pragma(_) => "pragma",
            };
            let name = d
                .name()
                .map(|n| hir.idents[n].text.clone())
                .unwrap_or_default();
            HirDecl {
                kind,
                name,
                range: d.byte_range().clone().into(),
            }
        })
        .collect();
    HirSummary {
        module_name: module.name,
        lib: module.lib,
        decls,
        counts: HirCounts {
            decls: hir.decls.len(),
            stmts: hir.stmts.len(),
            exprs: hir.exprs.len(),
            type_refs: hir.type_refs.len(),
            idents: hir.idents.len(),
        },
    }
}

#[wasm_bindgen]
pub fn lower_hir(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let hir = lower_module(source, "module", "project", tree.root_node());
    to_js(&summarize_hir(&hir))
}

// =============================================================================
// infer_types
// =============================================================================

#[derive(Serialize)]
struct ExprType {
    range: ByteRange,
    ty: String,
}

#[wasm_bindgen]
pub fn infer_types(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let hir = lower_module(source, "module", "project", tree.root_node());
    let resolutions = resolve(&hir);
    let analysis = analyze(&hir, &resolutions);

    let mut out = Vec::with_capacity(analysis.expr_types.len());
    for (idx, ty) in &analysis.expr_types {
        let range = hir.exprs[*idx].byte_range();
        out.push(ExprType {
            range: range.into(),
            ty: greycat_analyzer_types::display(&analysis.types, *ty),
        });
    }
    to_js(&out)
}

// =============================================================================
// diagnostics
// =============================================================================

#[derive(Serialize)]
struct WasmDiagnostic {
    severity: &'static str,
    source: &'static str,
    code: Option<String>,
    message: String,
    range: ByteRange,
    start: Position,
    end: Position,
}

#[wasm_bindgen]
pub fn diagnostics(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let mut out: Vec<WasmDiagnostic> = parse_diagnostics(tree.root_node(), source)
        .into_iter()
        .map(|d| {
            let r = byte_range_from_lsp(source, &d.range);
            WasmDiagnostic {
                severity: "error",
                source: "greycat-analyzer",
                code: code_string(&d.code),
                message: d.message,
                range: r.clone().into(),
                start: pos_at(source, r.start),
                end: pos_at(source, r.end),
            }
        })
        .collect();

    let hir = lower_module(source, "module", "project", tree.root_node());
    let resolutions = resolve(&hir);
    let analysis = analyze(&hir, &resolutions);
    for d in &analysis.diagnostics {
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Hint => "hint",
        };
        out.push(WasmDiagnostic {
            severity: sev,
            source: "greycat-analyzer",
            code: Some("semantic".into()),
            message: d.message.clone(),
            range: d.byte_range.clone().into(),
            start: pos_at(source, d.byte_range.start),
            end: pos_at(source, d.byte_range.end),
        });
    }
    for l in run_lints(&hir, &resolutions) {
        let sev = match l.severity {
            LintSeverity::Warning => "warning",
            LintSeverity::Hint => "hint",
        };
        out.push(WasmDiagnostic {
            severity: sev,
            source: "lint",
            code: Some(l.rule.to_string()),
            message: l.message,
            range: l.byte_range.clone().into(),
            start: pos_at(source, l.byte_range.start),
            end: pos_at(source, l.byte_range.end),
        });
    }
    to_js(&out)
}

fn byte_range_from_lsp(
    text: &str,
    range: &greycat_analyzer_core::lsp_types::Range,
) -> std::ops::Range<usize> {
    fn pos_to_byte(text: &str, p: greycat_analyzer_core::lsp_types::Position) -> usize {
        let mut line = 0u32;
        let mut byte = 0usize;
        for c in text.chars() {
            if line == p.line {
                break;
            }
            byte += c.len_utf8();
            if c == '\n' {
                line += 1;
            }
        }
        let mut col = 0u32;
        let bytes = text.as_bytes();
        while col < p.character && byte < bytes.len() {
            if bytes[byte] == b'\n' {
                break;
            }
            let c = text[byte..].chars().next().unwrap();
            byte += c.len_utf8();
            col += c.len_utf8() as u32;
        }
        byte
    }
    pos_to_byte(text, range.start)..pos_to_byte(text, range.end)
}

fn code_string(code: &Option<greycat_analyzer_core::lsp_types::NumberOrString>) -> Option<String> {
    code.as_ref().map(|c| match c {
        greycat_analyzer_core::lsp_types::NumberOrString::String(s) => s.clone(),
        greycat_analyzer_core::lsp_types::NumberOrString::Number(n) => n.to_string(),
    })
}

// =============================================================================
// format
// =============================================================================

#[wasm_bindgen]
pub fn format(source: &str) -> String {
    greycat_analyzer_fmt::format(source)
}
