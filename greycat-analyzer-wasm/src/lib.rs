// P5.1
//! WASM API surface for the greycat analyzer.
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

use std::str::FromStr;

use serde::Serialize;
use wasm_bindgen::prelude::*;

use greycat_analyzer_analysis::{
    analyzer::{Severity, analyze},
    lint::{LintSeverity, run_lints},
    project::ProjectAnalysis,
    resolver::resolve,
};
use greycat_analyzer_core::SourceManager;
use greycat_analyzer_core::SymbolTable;
use greycat_analyzer_core::diagnostics::parse_diagnostics;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
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
    greycat_analyzer_syntax::parse(source).root_node().to_sexp()
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
    fn walk<'a>(node: tree_sitter::Node<'a>, source: &str, out: &mut Vec<Token>) {
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

/// HIR node serialized for the playground. Same shape regardless of
/// whether the source variant is a decl, statement, expression, or
/// type-ref — the panel renders them uniformly as a foldable tree.
/// `kind` is human-readable (`"fn"`, `"stmt:var"`, `"expr:ident"`,
/// `"type-ref"`); `label` is the name / variant tag / ident text /
/// primitive name when relevant; `range` mirrors the source span.
#[derive(Serialize)]
struct HirNode {
    kind: String,
    label: Option<String>,
    range: ByteRange,
    children: Vec<HirNode>,
}

#[derive(Serialize)]
struct HirRoot {
    module_name: String,
    lib: String,
    counts: HirCounts,
    decls: Vec<HirNode>,
}

#[derive(Serialize)]
struct HirCounts {
    decls: usize,
    stmts: usize,
    exprs: usize,
    type_refs: usize,
    idents: usize,
}

fn ident_text(
    hir: &Hir,
    symbols: &SymbolTable,
    idx: Idx<greycat_analyzer_hir::types::Ident>,
) -> String {
    symbols[hir.idents[idx].symbol].to_string()
}

fn type_ref_node(
    hir: &Hir,
    symbols: &SymbolTable,
    idx: Idx<greycat_analyzer_hir::types::TypeRef>,
) -> HirNode {
    let tr = &hir.type_refs[idx];
    let mut children = Vec::new();
    for &p in &tr.params {
        children.push(type_ref_node(hir, symbols, p));
    }
    let mut label = String::new();
    for q in tr.qualifier.iter() {
        label.push_str(&ident_text(hir, symbols, *q));
        label.push_str("::");
    }
    label.push_str(&ident_text(hir, symbols, tr.name));
    if tr.optional {
        label.push('?');
    }
    HirNode {
        kind: "type-ref".into(),
        label: Some(label),
        range: tr.byte_range.clone().into(),
        children,
    }
}

fn expr_node(
    hir: &Hir,
    symbols: &SymbolTable,
    idx: Idx<greycat_analyzer_hir::types::Expr>,
) -> HirNode {
    use greycat_analyzer_hir::types::Expr;
    let e = &hir.exprs[idx];
    let range: ByteRange = e.byte_range().into();
    let (kind, label, children) = match e {
        Expr::Ident { name, .. } => (
            "expr:ident",
            Some(ident_text(hir, symbols, *name)),
            Vec::new(),
        ),
        Expr::Literal(l) => ("expr:literal", Some(format!("{:?}", l.kind)), Vec::new()),
        Expr::Null { .. } => ("expr:null", None, Vec::new()),
        Expr::This { .. } => ("expr:this", None, Vec::new()),
        Expr::String(s) => {
            let mut kids = Vec::new();
            for part in &s.parts {
                if let greycat_analyzer_hir::types::StringPart::Interp { expr, byte_range } = part {
                    kids.push(HirNode {
                        kind: "expr:string-interp".into(),
                        label: None,
                        range: byte_range.clone().into(),
                        children: vec![expr_node(hir, symbols, *expr)],
                    });
                }
            }
            ("expr:string", None, kids)
        }
        Expr::Tuple(items, _) => (
            "expr:tuple",
            None,
            items.iter().map(|i| expr_node(hir, symbols, *i)).collect(),
        ),
        Expr::Array(items, _) => (
            "expr:array",
            None,
            items.iter().map(|i| expr_node(hir, symbols, *i)).collect(),
        ),
        Expr::Object(o) => {
            let mut kids = Vec::new();
            if let Some(t) = o.ty {
                kids.push(type_ref_node(hir, symbols, t));
            }
            for f in &o.fields {
                kids.push(HirNode {
                    kind: "expr:object-field".into(),
                    label: f.name.map(|n| ident_text(hir, symbols, n)),
                    range: f.byte_range.clone().into(),
                    children: vec![expr_node(hir, symbols, f.value)],
                });
            }
            ("expr:object", None, kids)
        }
        Expr::Member(m) | Expr::Arrow(m) => {
            let kind = if matches!(e, Expr::Arrow(_)) {
                "expr:arrow"
            } else {
                "expr:member"
            };
            (
                kind,
                Some(ident_text(hir, symbols, m.property.ident())),
                vec![expr_node(hir, symbols, m.receiver)],
            )
        }
        Expr::Static(s) => (
            "expr:static",
            Some(ident_text(hir, symbols, s.property.ident())),
            vec![type_ref_node(hir, symbols, s.ty)],
        ),
        Expr::QualifiedStatic { chain, .. } => (
            "expr:qualified-static",
            Some(
                chain
                    .iter()
                    .map(|n| ident_text(hir, symbols, *n))
                    .collect::<Vec<_>>()
                    .join("::"),
            ),
            Vec::new(),
        ),
        Expr::Offset(o) => (
            "expr:offset",
            None,
            vec![
                expr_node(hir, symbols, o.receiver),
                expr_node(hir, symbols, o.index),
            ],
        ),
        Expr::Call(c) => {
            let mut kids = vec![expr_node(hir, symbols, c.callee)];
            for &a in &c.args {
                kids.push(expr_node(hir, symbols, a));
            }
            ("expr:call", None, kids)
        }
        Expr::Binary(b) => (
            "expr:binary",
            Some(format!("{:?}", b.op)),
            vec![
                expr_node(hir, symbols, b.left),
                expr_node(hir, symbols, b.right),
            ],
        ),
        Expr::Unary(u) => (
            "expr:unary",
            Some(format!("{:?}", u.op)),
            vec![expr_node(hir, symbols, u.operand)],
        ),
        Expr::Paren(inner, _) => ("expr:paren", None, vec![expr_node(hir, symbols, *inner)]),
        Expr::Lambda(l) => {
            let mut kids = Vec::new();
            for &p in &l.params {
                let pp = &hir.fn_params[p];
                let mut pk = Vec::new();
                if let Some(t) = pp.ty {
                    pk.push(type_ref_node(hir, symbols, t));
                }
                kids.push(HirNode {
                    kind: "fn-param".into(),
                    label: Some(ident_text(hir, symbols, pp.name)),
                    range: hir.idents[pp.name].byte_range.clone().into(),
                    children: pk,
                });
            }
            kids.push(expr_node(hir, symbols, l.body));
            ("expr:lambda", None, kids)
        }
        Expr::Is { value, ty, .. } => (
            "expr:is",
            None,
            vec![
                expr_node(hir, symbols, *value),
                type_ref_node(hir, symbols, *ty),
            ],
        ),
        Expr::Cast { value, ty, .. } => (
            "expr:cast",
            None,
            vec![
                expr_node(hir, symbols, *value),
                type_ref_node(hir, symbols, *ty),
            ],
        ),
        Expr::Range { from, to, .. } => {
            let mut kids = Vec::new();
            if let Some(f) = from {
                kids.push(expr_node(hir, symbols, *f));
            }
            if let Some(t) = to {
                kids.push(expr_node(hir, symbols, *t));
            }
            ("expr:range", None, kids)
        }
        Expr::Unsupported { kind, .. } => ("expr:unsupported", Some((*kind).into()), Vec::new()),
    };
    HirNode {
        kind: kind.into(),
        label,
        range,
        children,
    }
}

fn block_node(
    hir: &Hir,
    symbols: &SymbolTable,
    block: &greycat_analyzer_hir::types::BlockStmt,
) -> HirNode {
    HirNode {
        kind: "stmt:block".into(),
        label: None,
        range: block.byte_range.clone().into(),
        children: block
            .stmts
            .iter()
            .map(|s| stmt_node(hir, symbols, *s))
            .collect(),
    }
}

fn stmt_node(
    hir: &Hir,
    symbols: &SymbolTable,
    idx: Idx<greycat_analyzer_hir::types::Stmt>,
) -> HirNode {
    use greycat_analyzer_hir::types::Stmt;
    let s = &hir.stmts[idx];
    match s {
        Stmt::Block(b) => block_node(hir, symbols, b),
        Stmt::Expr(e) => HirNode {
            kind: "stmt:expr".into(),
            label: None,
            range: hir.exprs[*e].byte_range().into(),
            children: vec![expr_node(hir, symbols, *e)],
        },
        Stmt::Var(v) => {
            let mut kids = Vec::new();
            if let Some(t) = v.ty {
                kids.push(type_ref_node(hir, symbols, t));
            }
            if let Some(i) = v.init {
                kids.push(expr_node(hir, symbols, i));
            }
            HirNode {
                kind: "stmt:var".into(),
                label: Some(ident_text(hir, symbols, v.name)),
                range: v.byte_range.clone().into(),
                children: kids,
            }
        }
        Stmt::Assign(a) => HirNode {
            kind: "stmt:assign".into(),
            label: Some(format!("{:?}", a.op)),
            range: a.byte_range.clone().into(),
            children: vec![
                expr_node(hir, symbols, a.target),
                expr_node(hir, symbols, a.value),
            ],
        },
        Stmt::If(i) => {
            let mut kids = vec![
                expr_node(hir, symbols, i.condition),
                block_node(hir, symbols, &i.then_branch),
            ];
            if let Some(eb) = i.else_branch {
                kids.push(stmt_node(hir, symbols, eb));
            }
            HirNode {
                kind: "stmt:if".into(),
                label: None,
                range: i.byte_range.clone().into(),
                children: kids,
            }
        }
        Stmt::While(w) => HirNode {
            kind: "stmt:while".into(),
            label: None,
            range: w.byte_range.clone().into(),
            children: vec![
                expr_node(hir, symbols, w.condition),
                block_node(hir, symbols, &w.body),
            ],
        },
        Stmt::DoWhile(w) => HirNode {
            kind: "stmt:do-while".into(),
            label: None,
            range: w.byte_range.clone().into(),
            children: vec![
                block_node(hir, symbols, &w.body),
                expr_node(hir, symbols, w.condition),
            ],
        },
        Stmt::For(f) => {
            let mut kids = Vec::new();
            if let Some(v) = f.init_value {
                kids.push(expr_node(hir, symbols, v));
            }
            if let Some(c) = f.condition {
                kids.push(expr_node(hir, symbols, c));
            }
            if let Some(i) = f.increment {
                kids.push(expr_node(hir, symbols, i));
            }
            kids.push(block_node(hir, symbols, &f.body));
            HirNode {
                kind: "stmt:for".into(),
                label: f.init_name.map(|n| ident_text(hir, symbols, n)),
                range: f.byte_range.clone().into(),
                children: kids,
            }
        }
        Stmt::ForIn(f) => {
            let mut kids = Vec::new();
            for p in &f.params {
                let mut pk = Vec::new();
                if let Some(t) = p.ty {
                    pk.push(type_ref_node(hir, symbols, t));
                }
                kids.push(HirNode {
                    kind: "for-in-param".into(),
                    label: Some(ident_text(hir, symbols, p.name)),
                    range: hir.idents[p.name].byte_range.clone().into(),
                    children: pk,
                });
            }
            kids.push(expr_node(hir, symbols, f.range));
            kids.push(block_node(hir, symbols, &f.body));
            HirNode {
                kind: "stmt:for-in".into(),
                label: None,
                range: f.byte_range.clone().into(),
                children: kids,
            }
        }
        Stmt::Return(v) => HirNode {
            kind: "stmt:return".into(),
            label: None,
            range: v.map(|e| hir.exprs[e].byte_range()).unwrap_or(0..0).into(),
            children: v
                .map(|e| vec![expr_node(hir, symbols, e)])
                .unwrap_or_default(),
        },
        Stmt::Throw(e) => HirNode {
            kind: "stmt:throw".into(),
            label: None,
            range: hir.exprs[*e].byte_range().into(),
            children: vec![expr_node(hir, symbols, *e)],
        },
        Stmt::Break => HirNode {
            kind: "stmt:break".into(),
            label: None,
            range: (0..0).into(),
            children: Vec::new(),
        },
        Stmt::Continue => HirNode {
            kind: "stmt:continue".into(),
            label: None,
            range: (0..0).into(),
            children: Vec::new(),
        },
        Stmt::Breakpoint => HirNode {
            kind: "stmt:breakpoint".into(),
            label: None,
            range: (0..0).into(),
            children: Vec::new(),
        },
        Stmt::Try(t) => {
            let mut kids = vec![block_node(hir, symbols, &t.try_block)];
            kids.push(HirNode {
                kind: "catch".into(),
                label: t.error_param.map(|n| ident_text(hir, symbols, n)),
                range: t.catch_block.byte_range.clone().into(),
                children: vec![block_node(hir, symbols, &t.catch_block)],
            });
            HirNode {
                kind: "stmt:try".into(),
                label: None,
                range: t.byte_range.clone().into(),
                children: kids,
            }
        }
        Stmt::At(a) => HirNode {
            kind: "stmt:at".into(),
            label: None,
            range: a.byte_range.clone().into(),
            children: vec![
                expr_node(hir, symbols, a.expr),
                block_node(hir, symbols, &a.block),
            ],
        },
    }
}

fn decl_node(
    hir: &Hir,
    symbols: &SymbolTable,
    idx: Idx<greycat_analyzer_hir::types::Decl>,
) -> HirNode {
    use greycat_analyzer_hir::types::Decl;
    let d = &hir.decls[idx];
    let range: ByteRange = d.byte_range().clone().into();
    let name = d.name().map(|n| ident_text(hir, symbols, n));
    match d {
        Decl::Fn(f) => {
            let mut kids = Vec::new();
            for &g in &f.generics {
                kids.push(HirNode {
                    kind: "generic".into(),
                    label: Some(ident_text(hir, symbols, g)),
                    range: hir.idents[g].byte_range.clone().into(),
                    children: Vec::new(),
                });
            }
            for &p in &f.params {
                let pp = &hir.fn_params[p];
                let mut pk = Vec::new();
                if let Some(t) = pp.ty {
                    pk.push(type_ref_node(hir, symbols, t));
                }
                kids.push(HirNode {
                    kind: "fn-param".into(),
                    label: Some(ident_text(hir, symbols, pp.name)),
                    range: hir.idents[pp.name].byte_range.clone().into(),
                    children: pk,
                });
            }
            if let Some(rt) = f.return_type {
                kids.push(HirNode {
                    kind: "fn-return-type".into(),
                    label: None,
                    range: hir.type_refs[rt].byte_range.clone().into(),
                    children: vec![type_ref_node(hir, symbols, rt)],
                });
            }
            if let Some(body) = f.body {
                kids.push(stmt_node(hir, symbols, body));
            }
            HirNode {
                kind: "fn".into(),
                label: name,
                range,
                children: kids,
            }
        }
        Decl::Type(t) => {
            let mut kids = Vec::new();
            for &g in &t.generics {
                kids.push(HirNode {
                    kind: "generic".into(),
                    label: Some(ident_text(hir, symbols, g)),
                    range: hir.idents[g].byte_range.clone().into(),
                    children: Vec::new(),
                });
            }
            if let Some(parent) = t.supertype {
                kids.push(HirNode {
                    kind: "extends".into(),
                    label: None,
                    range: hir.type_refs[parent].byte_range.clone().into(),
                    children: vec![type_ref_node(hir, symbols, parent)],
                });
            }
            for &a in &t.attrs {
                let attr = &hir.type_attrs[a];
                let mut ak = Vec::new();
                if let Some(ty) = attr.ty {
                    ak.push(type_ref_node(hir, symbols, ty));
                }
                if let Some(init) = attr.init {
                    ak.push(expr_node(hir, symbols, init));
                }
                kids.push(HirNode {
                    kind: "type-attr".into(),
                    label: Some(ident_text(hir, symbols, attr.name)),
                    range: attr.byte_range.clone().into(),
                    children: ak,
                });
            }
            for &m in &t.methods {
                kids.push(decl_node(hir, symbols, m));
            }
            HirNode {
                kind: "type".into(),
                label: name,
                range,
                children: kids,
            }
        }
        Decl::Enum(e) => {
            let kids = e
                .fields
                .iter()
                .map(|&f| {
                    let ef = &hir.enum_fields[f];
                    let mut fk = Vec::new();
                    if let Some(v) = ef.value {
                        fk.push(expr_node(hir, symbols, v));
                    }
                    HirNode {
                        kind: "enum-variant".into(),
                        label: Some(ident_text(hir, symbols, ef.name)),
                        range: hir.idents[ef.name].byte_range.clone().into(),
                        children: fk,
                    }
                })
                .collect();
            HirNode {
                kind: "enum".into(),
                label: name,
                range,
                children: kids,
            }
        }
        Decl::Var(v) => {
            let mut kids = Vec::new();
            if let Some(t) = v.ty {
                kids.push(type_ref_node(hir, symbols, t));
            }
            if let Some(i) = v.init {
                kids.push(expr_node(hir, symbols, i));
            }
            HirNode {
                kind: "var".into(),
                label: name,
                range,
                children: kids,
            }
        }
        Decl::Pragma(p) => {
            let kids = p.args.iter().map(|&a| expr_node(hir, symbols, a)).collect();
            HirNode {
                kind: "pragma".into(),
                label: Some(ident_text(hir, symbols, p.name)),
                range,
                children: kids,
            }
        }
    }
}

fn full_hir(hir: &Hir, symbols: &SymbolTable) -> HirRoot {
    let module = hir
        .module
        .clone()
        .unwrap_or_else(|| greycat_analyzer_hir::types::Module {
            name: "<empty>".into(),
            lib: "project".into(),
            decls: Box::default(),
            byte_range: 0..0,
        });
    let decls = module
        .decls
        .iter()
        .map(|&d| decl_node(hir, symbols, d))
        .collect();
    HirRoot {
        module_name: module.name,
        lib: module.lib,
        counts: HirCounts {
            decls: hir.decls.len(),
            stmts: hir.stmts.len(),
            exprs: hir.exprs.len(),
            type_refs: hir.type_refs.len(),
            idents: hir.idents.len(),
        },
        decls,
    }
}

#[wasm_bindgen]
pub fn lower_hir(source: &str) -> Result<JsValue, JsValue> {
    let tree = greycat_analyzer_syntax::parse(source);
    let symbols = SymbolTable::new();
    let hir = lower_module(source, &symbols, "module", "project", tree.root_node());
    to_js(&full_hir(&hir, &symbols))
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
    // Route through `ProjectAnalysis` so the cross-module type fixups
    // (P16.3 / P16.4) and `validate_type_relations` post-pass settle
    // before we read `expr_types`. The earlier per-module
    // `analyze(&hir, &res)` path returned every cross-module call's
    // result as `any` (the post-pass writes those back) — so the
    // playground's Types panel only ever showed `any`. The single-doc
    // SourceManager still goes through the same pipeline; cross-module
    // fixups are no-ops with one module but `validate_type_relations`
    // and the typed-lint pass run normally.
    let mut mgr = SourceManager::new();
    let uri = Uri::from_str("file:///playground.gcl")
        .map_err(|e| JsValue::from_str(&format!("uri: {e}")))?;
    mgr.add_simple(uri.clone(), source, "project", false);
    let pa = ProjectAnalysis::analyze(&mgr);
    let module = match pa.module(&uri) {
        Some(m) => m,
        None => return to_js(&Vec::<ExprType>::new()),
    };
    let mut out = Vec::with_capacity(module.analysis.expr_types.len());
    for (idx, ty) in &module.analysis.expr_types {
        let range = module.hir.exprs[*idx].byte_range();
        out.push(ExprType {
            range: range.into(),
            ty: pa.display_type(*ty).to_string(),
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

    let symbols = SymbolTable::new();
    let hir = lower_module(source, &symbols, "module", "project", tree.root_node());
    let resolutions = resolve(&hir, &symbols);
    let (_arena, _decl_registry, analysis) = analyze(&hir, &resolutions, &symbols);
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
    for l in run_lints(&hir, &resolutions, &symbols) {
        let sev = match l.severity {
            LintSeverity::Error => "error",
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
