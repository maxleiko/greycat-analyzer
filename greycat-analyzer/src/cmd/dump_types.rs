// P18.1 — oracle subcommands. P18.2-18.4 — parity gauntlet.
//! `greycat-lang dump-types` / `dump-resolutions` — typed-AST parity
//! oracle subcommands.
//!
//! Emits one JSONL record per typed expression / type-reference
//! (`dump-types`) or per ident use (`dump-resolutions`), matching the
//! TS reference's shape so the parity gauntlet can diff
//! the two outputs directly.
//!
//! `dump-types` record (per line):
//! - `file` — path relative to the project root.
//! - `range` — `[start, end]` UTF-8 byte half-open.
//! - `line`, `col`, `endLine`, `endCol` — line is 1-based, col is 0-based.
//! - `kind` — TS-side CST kind name (`Identifier`, `InstanceAccessExpr`, …).
//! - `type` — fully-qualified canonical form (`core::int`, `project::Foo`, …).
//! - `nullable` — mirrors `Type.nullable`.
//! - `text` — source slice for the node.
//!
//! `dump-resolutions` record adds `refKind` / `declKind` / `name` /
//! `decl: { fqn, file, range, line, col }` instead of `kind` / `type` /
//! `nullable` / `text`.
//!
//! Records are sorted by `(file, line, col, byteStart, byteEnd)` so the
//! diff against TS stays stable.

use std::{
    cell::Ref,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use greycat_analyzer_analysis::{
    project::{ModuleAnalysis, ProjectAnalysis},
    resolver::{Definition, Resolutions},
    stdlib::ProjectIndex,
};
use greycat_analyzer_core::{Document, SourceManager, lsp_types::Uri, resolver::FsContext};
use greycat_analyzer_hir::{
    Hir,
    arena::Idx,
    types::{Decl, Expr, LiteralKind, Pragma, StringPart, TypeRef, UnaryOp},
};
use greycat_analyzer_types::{Primitive, TypeArena, TypeId, display_fqn};

use crate::utils::AnyError;

#[derive(clap::Parser)]
#[clap(
    about = "Dump per-expression byte ranges and inferred type display strings as JSONL.\n\
            Records are sorted by (file, line, col, byteStart, byteEnd) so the parity\n\
            gauntlet can diff this output against the TS reference's `dump-types`."
)]
pub struct DumpTypes {
    #[clap(help = "Path to a project.gcl entrypoint, a project directory \
                containing project.gcl, or a single .gcl file. Single-file \
                mode walks up to the project root, analyzes the whole \
                project for cross-module bindings, then scopes output to \
                just the input file.")]
    target: PathBuf,
    #[clap(
        long,
        help = "Restrict output to a range. Format: 'B' (byte offset), \
                'B-B' (byte range), 'L:C' (line:col, 1-based line, 0-based \
                col), or 'L:C-L:C'."
    )]
    filter: Option<String>,
}

impl DumpTypes {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();
        run_dump(&self.target, self.filter.as_deref(), Mode::Types)
    }
}

#[derive(clap::Parser)]
#[clap(about = "Dump per-ident-use byte ranges and decl pointers as JSONL.\n\
            Records are sorted by (file, line, col, byteStart, byteEnd).")]
pub struct DumpResolutions {
    #[clap(help = "Path to a project.gcl entrypoint, a project directory, or \
                a single .gcl file. Single-file mode scopes output to just \
                the input file but still analyzes the closure for \
                cross-module decls.")]
    target: PathBuf,
    #[clap(
        long,
        help = "Restrict output to a range. Format: 'B', 'B-B', 'L:C', \
                or 'L:C-L:C'."
    )]
    filter: Option<String>,
}

impl DumpResolutions {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();
        run_dump(&self.target, self.filter.as_deref(), Mode::Resolutions)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Types,
    Resolutions,
}

fn run_dump(target: &Path, filter: Option<&str>, mode: Mode) -> Result<ExitCode, AnyError> {
    let canonical = target.canonicalize()?;
    let (project_root, single_file_rel) = resolve_project(&canonical)?;
    let project_gcl = project_root.join("project.gcl");
    if !project_gcl.is_file() {
        eprintln!(
            "error: no project.gcl found at {} (looked for the project root by walking up from {})",
            project_gcl.display(),
            canonical.display(),
        );
        return Ok(ExitCode::FAILURE);
    }

    let ctx = FsContext::new().unwrap_or_else(|_| FsContext::with_greycat_home(PathBuf::new()));
    let mut mgr = SourceManager::with_context(Arc::new(ctx));
    let _report = mgr.load_project(&project_gcl);
    let analysis = ProjectAnalysis::analyze(&mgr);

    let parsed_filter = match filter {
        Some(s) => Some(Filter::parse(s)?),
        None => None,
    };

    let mut records: Vec<Record> = Vec::new();
    for (uri, module) in analysis.iter() {
        let Some(cell) = mgr.get(uri) else {
            continue;
        };
        let doc = cell.borrow();
        if doc.lib != "project" {
            continue;
        }
        let path_buf = uri_to_pathbuf(uri).unwrap_or_else(|| PathBuf::from(uri.as_str()));
        let rel = relative_to(&project_root, &path_buf);
        if let Some(only) = single_file_rel.as_ref()
            && &rel != only
        {
            continue;
        }
        match mode {
            Mode::Types => {
                collect_type_records(
                    &rel,
                    &doc,
                    module,
                    analysis.arena(),
                    &analysis.index,
                    &mut records,
                );
            }
            Mode::Resolutions => {
                collect_resolution_records(
                    &rel,
                    &doc,
                    module,
                    &mgr,
                    &project_root,
                    &analysis.index,
                    &mut records,
                );
            }
        }
    }

    records.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
            .then(a.range_start.cmp(&b.range_start))
            .then(a.range_end.cmp(&b.range_end))
    });

    if let Some(filter) = parsed_filter.as_ref() {
        records.retain(|r| filter.matches(r));
    }

    use std::io::Write;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for r in &records {
        writeln!(handle, "{}", r.json)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Record shape
// ---------------------------------------------------------------------------

struct Record {
    file: String,
    line: u32,
    col: u32,
    range_start: usize,
    range_end: usize,
    json: String,
}

// ---------------------------------------------------------------------------
// Project resolution
// ---------------------------------------------------------------------------

fn resolve_project(target: &Path) -> Result<(PathBuf, Option<PathBuf>), AnyError> {
    if target.is_dir() {
        return Ok((target.to_path_buf(), None));
    }
    let parent = target.parent().unwrap_or(Path::new("."));
    if target.file_name().and_then(|s| s.to_str()) == Some("project.gcl") {
        return Ok((parent.to_path_buf(), None));
    }
    let mut cur = Some(parent);
    while let Some(d) = cur {
        if d.join("project.gcl").is_file() {
            let rel = relative_to(d, target);
            return Ok((d.to_path_buf(), Some(rel)));
        }
        cur = d.parent();
    }
    let rel = relative_to(parent, target);
    Ok((parent.to_path_buf(), Some(rel)))
}

fn relative_to(root: &Path, p: &Path) -> PathBuf {
    p.strip_prefix(root).unwrap_or(p).to_path_buf()
}

fn uri_to_pathbuf(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

fn module_stem_from_uri(uri: &Uri) -> Option<String> {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    let last = stripped.rsplit(['/', '\\']).next()?;
    let stem = last.strip_suffix(".gcl").unwrap_or(last);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Maps a *named* type (`Foo`, `node`, `String`) to its home library
/// stem — `project::Foo` for `project.gcl`, `runtime::Identity` for
/// `runtime.gcl`, etc. Returns `None` for builtins / unresolved names;
/// callers fall back to `core` (matches TS).
fn home_lib_for(index: &ProjectIndex, name: &str) -> Option<String> {
    let locs = index.locate_decl(name);
    locs.first().and_then(|(uri, _)| module_stem_from_uri(uri))
}

// ---------------------------------------------------------------------------
// Filter parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum FilterKind {
    Byte(usize, usize),
    LineCol(u32, u32, u32, u32),
}

#[derive(Debug, Clone, Copy)]
struct Filter {
    kind: FilterKind,
}

impl Filter {
    fn parse(s: &str) -> Result<Self, AnyError> {
        let s = s.trim();
        if let Some((left, right)) = s.split_once('-') {
            if let (Some(a), Some(b)) = (parse_lc(left), parse_lc(right)) {
                return Ok(Filter {
                    kind: FilterKind::LineCol(a.0, a.1, b.0, b.1),
                });
            }
            let a: usize = left.parse()?;
            let b: usize = right.parse()?;
            return Ok(Filter {
                kind: FilterKind::Byte(a, b),
            });
        }
        if let Some(a) = parse_lc(s) {
            return Ok(Filter {
                kind: FilterKind::LineCol(a.0, a.1, a.0, a.1),
            });
        }
        let b: usize = s.parse()?;
        Ok(Filter {
            kind: FilterKind::Byte(b, b),
        })
    }

    fn matches(&self, r: &Record) -> bool {
        match self.kind {
            FilterKind::Byte(a, b) => r.range_start.max(a) < r.range_end.min(b + 1),
            FilterKind::LineCol(la, ca, lb, cb) => {
                let start = (r.line, r.col);
                let lo = (la, ca);
                let hi = (lb, cb);
                start >= lo && start <= hi
            }
        }
    }
}

fn parse_lc(s: &str) -> Option<(u32, u32)> {
    let (l, c) = s.split_once(':')?;
    let line: u32 = l.parse().ok()?;
    let col: u32 = c.parse().ok()?;
    Some((line, col))
}

// ---------------------------------------------------------------------------
// Type-records collection
// ---------------------------------------------------------------------------

fn collect_type_records(
    rel: &Path,
    doc: &Ref<'_, Document>,
    module: &ModuleAnalysis,
    project_arena: &greycat_analyzer_types::TypeArena,
    index: &ProjectIndex,
    out: &mut Vec<Record>,
) {
    let file = path_to_string(rel);
    let text = &doc.text;
    let hir = &module.hir;
    let analysis = &module.analysis;

    // P19 — clone the project arena so `lower_type_ref_local` can
    // mint without disturbing the cached project state. Cloning
    // preserves intern keys, so TypeIds from `analysis.expr_types`
    // remain valid in the local copy.
    let mut arena = project_arena.clone();
    let any_id = arena.any();
    let home = |n: &str| home_lib_for(index, n);

    // 1. Per-expression records. Only emit for expressions the
    // analyzer typed — and skip pragma argument expressions: the TS
    // reference doesn't dump those (they're config strings, not part
    // of the typed program).
    let _ = any_id;
    let pragma_arg_exprs = collect_pragma_arg_exprs(hir);
    for (idx, expr) in hir.exprs.iter() {
        if pragma_arg_exprs.contains(&idx) {
            continue;
        }
        let Some(&ty) = analysis.expr_types.get(&idx) else {
            continue;
        };
        let Some((kind, byte_range)) = expr_kind_and_range(hir, expr) else {
            continue;
        };
        push_type_record(
            out,
            &file,
            text,
            &byte_range,
            kind,
            display_fqn(&arena, ty, &home),
            arena.get(ty).nullable,
        );
        // 1b. P17.5 — for template strings, also emit per-part
        // records (`RawStringExpr` for each fragment, `InterpolationExpr`
        // for each `${expr}`). The inner expr of `Interp` is in the
        // arena and emits its own record from this same loop.
        if let Expr::String(s) = expr
            && s.has_interpolation()
        {
            let str_ty_display = display_fqn(&arena, ty, &home);
            let str_ty_nullable = arena.get(ty).nullable;
            for part in &s.parts {
                match part {
                    StringPart::Lit { byte_range: br, .. } => {
                        push_type_record(
                            out,
                            &file,
                            text,
                            br,
                            "RawStringExpr",
                            str_ty_display.clone(),
                            str_ty_nullable,
                        );
                    }
                    StringPart::Interp { byte_range: br, .. } => {
                        push_type_record(
                            out,
                            &file,
                            text,
                            br,
                            "InterpolationExpr",
                            str_ty_display.clone(),
                            str_ty_nullable,
                        );
                    }
                }
            }
        }
    }

    // 2. Per-type-ref records (`TypeIdent` in TS).
    for (idx, _) in hir.type_refs.iter() {
        let tref = &hir.type_refs[idx];
        let ty = lower_type_ref_local(hir, idx, &mut arena);
        push_type_record(
            out,
            &file,
            text,
            &tref.byte_range,
            "TypeIdent",
            display_fqn(&arena, ty, &home),
            arena.get(ty).nullable,
        );
    }
}

fn push_type_record(
    out: &mut Vec<Record>,
    file: &str,
    text: &str,
    byte_range: &std::ops::Range<usize>,
    kind: &str,
    ty: String,
    nullable: bool,
) {
    if byte_range.start > text.len() || byte_range.end > text.len() {
        return;
    }
    if byte_range.start > byte_range.end {
        return;
    }
    let slice = &text[byte_range.start..byte_range.end];
    let (line, col) = line_col(text, byte_range.start);
    let (end_line, end_col) = line_col(text, byte_range.end);
    let json = format!(
        "{{\"file\":{f},\"range\":[{rs},{re}],\"line\":{line},\"col\":{col},\"endLine\":{el},\"endCol\":{ec},\"kind\":{k},\"type\":{t},\"nullable\":{n},\"text\":{txt}}}",
        f = json_string(file),
        rs = byte_range.start,
        re = byte_range.end,
        el = end_line,
        ec = end_col,
        k = json_string(kind),
        t = json_string(&ty),
        n = nullable,
        txt = json_string(slice),
    );
    out.push(Record {
        file: file.to_string(),
        line,
        col,
        range_start: byte_range.start,
        range_end: byte_range.end,
        json,
    });
}

/// TS-side CST kind for a HIR `Expr` plus its byte range (which may
/// come from the wrapped `Ident` for `Expr::Ident`).
fn expr_kind_and_range(hir: &Hir, expr: &Expr) -> Option<(&'static str, std::ops::Range<usize>)> {
    let kind: &'static str = match expr {
        Expr::Ident(id) => {
            let ident = &hir.idents[*id];
            return Some(("Identifier", ident.byte_range.clone()));
        }
        Expr::Literal(l) => match l.kind {
            LiteralKind::Number => "NumLit",
            LiteralKind::Char => "CharLit",
            LiteralKind::Bool => "BoolLit",
            LiteralKind::Null => "NullLit",
            LiteralKind::This => "ThisLit",
            LiteralKind::Duration => "NumLit",
            LiteralKind::Time => "NumLit",
            LiteralKind::Iso8601 => "StringLit",
        },
        Expr::String(s) => {
            if s.has_interpolation() {
                "TemplateExpr"
            } else {
                "StringLit"
            }
        }
        Expr::Tuple(..) => "ArrayExpr",
        Expr::Array(..) => "ArrayExpr",
        Expr::Object(_) => "ObjectExpr",
        Expr::Member(_) => "InstanceAccessExpr",
        Expr::Arrow(_) => "RefAccessExpr",
        Expr::Static(_) => "StaticAccessExpr",
        Expr::QualifiedStatic { .. } => "StaticAccessExpr",
        Expr::Offset(_) => "OffsetAccessExpr",
        Expr::Call(_) => "CallExpr",
        Expr::Binary(_) => "BinOpExpr",
        Expr::Unary(u) => match u.op {
            UnaryOp::NonNullAssert => "NonNullAssertExpr",
            UnaryOp::Neg | UnaryOp::Not | UnaryOp::BitNot | UnaryOp::Deref => "PrefixExpr",
        },
        Expr::Paren(..) => return None,
        Expr::Lambda(_) => "LambdaExpr",
        Expr::Is { .. } => "IsExpr",
        Expr::Cast { .. } => "AsExpr",
        Expr::Range { .. } => "RangeExpr",
        Expr::Unsupported { .. } => return None,
    };
    Some((kind, expr.byte_range()))
}

/// Collect every `Idx<Expr>` reachable from a pragma's args. The TS
/// reference's `dump-types` ignores pragma string args (their `type`
/// field is `null` in `dump-hir`); skipping them here keeps the
/// parity oracle clean.
fn collect_pragma_arg_exprs(hir: &Hir) -> std::collections::HashSet<Idx<Expr>> {
    let mut out = std::collections::HashSet::new();
    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    for d in &module.decls {
        let Decl::Pragma(p) = &hir.decls[*d] else {
            continue;
        };
        let _: &Pragma = p;
        for arg in &p.args {
            collect_expr_descendants(hir, *arg, &mut out);
        }
    }
    out
}

fn collect_expr_descendants(
    hir: &Hir,
    root: Idx<Expr>,
    out: &mut std::collections::HashSet<Idx<Expr>>,
) {
    if !out.insert(root) {
        return;
    }
    match &hir.exprs[root] {
        Expr::Ident(_) | Expr::Literal(_) => {}
        Expr::String(s) => {
            for part in &s.parts {
                if let StringPart::Interp { expr, .. } = part {
                    collect_expr_descendants(hir, *expr, out);
                }
            }
        }
        Expr::Tuple(items, _) | Expr::Array(items, _) => {
            for it in items {
                collect_expr_descendants(hir, *it, out);
            }
        }
        Expr::Object(o) => {
            for f in &o.fields {
                collect_expr_descendants(hir, f.value, out);
            }
        }
        Expr::Member(m) | Expr::Arrow(m) => {
            collect_expr_descendants(hir, m.receiver, out);
        }
        Expr::Static(_) | Expr::QualifiedStatic { .. } => {}
        Expr::Offset(o) => {
            collect_expr_descendants(hir, o.receiver, out);
            collect_expr_descendants(hir, o.index, out);
        }
        Expr::Call(c) => {
            collect_expr_descendants(hir, c.callee, out);
            for a in &c.args {
                collect_expr_descendants(hir, *a, out);
            }
        }
        Expr::Binary(b) => {
            collect_expr_descendants(hir, b.left, out);
            collect_expr_descendants(hir, b.right, out);
        }
        Expr::Unary(u) => {
            collect_expr_descendants(hir, u.operand, out);
        }
        Expr::Paren(inner, _) => {
            collect_expr_descendants(hir, *inner, out);
        }
        Expr::Lambda(_) => {}
        Expr::Is { value, .. } | Expr::Cast { value, .. } => {
            collect_expr_descendants(hir, *value, out);
        }
        Expr::Range { from, to, .. } => {
            if let Some(f) = from {
                collect_expr_descendants(hir, *f, out);
            }
            if let Some(t) = to {
                collect_expr_descendants(hir, *t, out);
            }
        }
        Expr::Unsupported { .. } => {}
    }
}

/// Local copy of `analysis::stdlib::lower_type_ref` (private upstream).
fn lower_type_ref_local(hir: &Hir, idx: Idx<TypeRef>, arena: &mut TypeArena) -> TypeId {
    let tr = &hir.type_refs[idx];
    let name = hir.idents[tr.name].text.to_string();
    let mut base = match name.as_str() {
        "bool" => arena.primitive(Primitive::Bool),
        "int" => arena.primitive(Primitive::Int),
        "float" => arena.primitive(Primitive::Float),
        "char" => arena.primitive(Primitive::Char),
        "String" => arena.primitive(Primitive::String),
        "time" => arena.primitive(Primitive::Time),
        "duration" => arena.primitive(Primitive::Duration),
        "geo" => arena.primitive(Primitive::Geo),
        "any" => arena.any(),
        "null" => arena.null(),
        _ => {
            if !tr.params.is_empty() {
                let args: Vec<TypeId> = tr
                    .params
                    .iter()
                    .map(|p| lower_type_ref_local(hir, *p, arena))
                    .collect();
                arena.generic(name, args)
            } else {
                arena.named(name)
            }
        }
    };
    if tr.optional {
        base = arena.nullable(base);
    }
    base
}

// ---------------------------------------------------------------------------
// Resolution-records collection
// ---------------------------------------------------------------------------

fn collect_resolution_records(
    rel: &Path,
    doc: &Ref<'_, Document>,
    module: &ModuleAnalysis,
    mgr: &SourceManager,
    project_root: &Path,
    index: &ProjectIndex,
    out: &mut Vec<Record>,
) {
    let file = path_to_string(rel);
    let text = &doc.text;
    let hir = &module.hir;
    let res = &module.resolutions;
    let lib_stem = home_lib_for_self_bindings(rel);

    // 1. Self-bindings for decl idents — TS emits a record at every
    // declaration site (fn name, type name, var name, fn param, local
    // var, catch param) pointing at the same span. Our resolver only
    // tracks *use* sites in `Resolutions.uses`, so we walk the decl
    // tree and synthesize the self-records ourselves.
    let self_decls = collect_decl_idents(hir);
    for (ident_idx, ref_kind, decl_kind) in self_decls {
        let ident = &hir.idents[ident_idx];
        let fqn = match decl_kind {
            "FnDecl" | "TypeDecl" | "EnumDecl" | "ModuleVar" => {
                format!("{lib_stem}::{}", ident.text)
            }
            _ => ident.text.to_string(),
        };
        let payload = DeclPayload {
            ref_kind,
            decl_kind,
            name: ident.text.to_string(),
            fqn,
            decl_file: file.clone(),
            decl_range: ident.byte_range.clone(),
            decl_line: 0,
            decl_col: 0,
        };
        push_resolution_record(out, &file, text, &ident.byte_range, &payload);
    }

    // 2. Use-site bindings from the resolver.
    for (ident_idx, def) in res.uses.iter() {
        let ident = &hir.idents[*ident_idx];
        let Some(payload) = build_decl_payload(def, hir, &file, mgr, project_root, index) else {
            continue;
        };
        push_resolution_record(out, &file, text, &ident.byte_range, &payload);
    }
}

/// The library/module stem used in self-binding FQN payloads (`project`
/// for project.gcl, `runtime` for runtime.gcl, …). Falls back to the
/// file stem of the relative path if it can't be derived.
fn home_lib_for_self_bindings(rel: &Path) -> String {
    rel.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "project".to_string())
}

/// Walks the HIR decl tree and yields every "binding site" ident — the
/// ones the TS reference's `dump-resolutions` records as self-bindings.
fn collect_decl_idents(
    hir: &Hir,
) -> Vec<(
    Idx<greycat_analyzer_hir::types::Ident>,
    &'static str,
    &'static str,
)> {
    let mut out = Vec::new();
    let Some(module) = hir.module.as_ref() else {
        return out;
    };
    for d in &module.decls {
        match &hir.decls[*d] {
            Decl::Fn(fnd) => {
                out.push((fnd.name, "fn", "FnDecl"));
                for p in &fnd.params {
                    out.push((hir.fn_params[*p].name, "var", "FnParam"));
                }
                if let Some(body) = fnd.body {
                    collect_stmt_decl_idents(hir, body, &mut out);
                }
            }
            Decl::Type(td) => {
                out.push((td.name, "type", "TypeDecl"));
                for m in &td.methods {
                    if let Decl::Fn(fnd) = &hir.decls[*m] {
                        out.push((fnd.name, "fn", "FnDecl"));
                        for p in &fnd.params {
                            out.push((hir.fn_params[*p].name, "var", "FnParam"));
                        }
                        if let Some(body) = fnd.body {
                            collect_stmt_decl_idents(hir, body, &mut out);
                        }
                    }
                }
            }
            Decl::Enum(ed) => {
                out.push((ed.name, "type", "EnumDecl"));
            }
            Decl::Var(vd) => {
                out.push((vd.name, "var", "ModuleVar"));
            }
            Decl::Pragma(_) => {}
        }
    }
    out
}

fn collect_block_decl_idents(
    hir: &Hir,
    block: &greycat_analyzer_hir::types::BlockStmt,
    out: &mut Vec<(
        Idx<greycat_analyzer_hir::types::Ident>,
        &'static str,
        &'static str,
    )>,
) {
    for s in &block.stmts {
        collect_stmt_decl_idents(hir, *s, out);
    }
}

fn collect_stmt_decl_idents(
    hir: &Hir,
    stmt_idx: Idx<greycat_analyzer_hir::types::Stmt>,
    out: &mut Vec<(
        Idx<greycat_analyzer_hir::types::Ident>,
        &'static str,
        &'static str,
    )>,
) {
    use greycat_analyzer_hir::types::Stmt;
    match &hir.stmts[stmt_idx] {
        Stmt::Block(b) => collect_block_decl_idents(hir, b, out),
        Stmt::Var(v) => {
            out.push((v.name, "var", "VarDecl"));
        }
        Stmt::If(i) => {
            collect_block_decl_idents(hir, &i.then_branch, out);
            if let Some(e) = i.else_branch {
                collect_stmt_decl_idents(hir, e, out);
            }
        }
        Stmt::While(w) => collect_block_decl_idents(hir, &w.body, out),
        Stmt::DoWhile(d) => collect_block_decl_idents(hir, &d.body, out),
        Stmt::For(f) => {
            if let Some(name) = f.init_name {
                out.push((name, "var", "VarDecl"));
            }
            collect_block_decl_idents(hir, &f.body, out);
        }
        Stmt::ForIn(fi) => {
            for p in &fi.params {
                out.push((p.name, "var", "VarDecl"));
            }
            collect_block_decl_idents(hir, &fi.body, out);
        }
        Stmt::Try(t) => {
            collect_block_decl_idents(hir, &t.try_block, out);
            if let Some(p) = t.error_param {
                out.push((p, "var", "CatchParam"));
            }
            collect_block_decl_idents(hir, &t.catch_block, out);
        }
        Stmt::Expr(_)
        | Stmt::Assign(_)
        | Stmt::Return(_)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::Throw(_)
        | Stmt::At(_) => {}
    }
}

struct DeclPayload {
    ref_kind: &'static str,
    decl_kind: &'static str,
    name: String,
    fqn: String,
    decl_file: String,
    decl_range: std::ops::Range<usize>,
    decl_line: u32,
    decl_col: u32,
}

fn build_decl_payload(
    def: &Definition,
    hir: &Hir,
    self_file: &str,
    mgr: &SourceManager,
    project_root: &Path,
    index: &ProjectIndex,
) -> Option<DeclPayload> {
    match def {
        Definition::Decl(decl_idx) => {
            let decl = &hir.decls[*decl_idx];
            let (rk, dk, name) = decl_summary(hir, decl);
            let name_id = decl.name()?;
            let id = &hir.idents[name_id];
            // We need the file's text to compute line/col — but we only
            // have it via the caller. This path is the same-file case,
            // so the caller's `text` is what we want. We approximate by
            // pushing the line/col computation up to the caller via a
            // marker; for now, we re-derive from the decl ident's
            // byte_range plus a deferred line/col.
            // TS reports the decl's *name* span (not the whole decl).
            let lib = home_lib_for(index, &name)
                .or_else(|| Some("project".to_string()))
                .unwrap_or_else(|| "core".to_string());
            let fqn = match dk {
                "TypeDecl" | "EnumDecl" | "FnDecl" | "ModuleVar" => format!("{lib}::{name}"),
                _ => name.clone(),
            };
            Some(DeclPayload {
                ref_kind: rk,
                decl_kind: dk,
                name,
                fqn,
                decl_file: self_file.to_string(),
                decl_range: id.byte_range.clone(),
                // Caller fills these in from the file's text.
                decl_line: 0,
                decl_col: 0,
            })
        }
        Definition::Local(idx) => {
            let id = &hir.idents[*idx];
            Some(DeclPayload {
                ref_kind: "var",
                decl_kind: "VarDecl",
                name: id.text.to_string(),
                fqn: id.text.to_string(),
                decl_file: self_file.to_string(),
                decl_range: id.byte_range.clone(),
                decl_line: 0,
                decl_col: 0,
            })
        }
        Definition::Param(idx) => {
            let id = &hir.idents[*idx];
            Some(DeclPayload {
                ref_kind: "var",
                decl_kind: "FnParam",
                name: id.text.to_string(),
                fqn: id.text.to_string(),
                decl_file: self_file.to_string(),
                decl_range: id.byte_range.clone(),
                decl_line: 0,
                decl_col: 0,
            })
        }
        Definition::Generic(idx) => {
            let id = &hir.idents[*idx];
            Some(DeclPayload {
                ref_kind: "type",
                decl_kind: "TypeParam",
                name: id.text.to_string(),
                fqn: id.text.to_string(),
                decl_file: self_file.to_string(),
                decl_range: id.byte_range.clone(),
                decl_line: 0,
                decl_col: 0,
            })
        }
        Definition::ProjectDecl { uri, decl } => {
            let other_cell = mgr.get(uri)?;
            let other_doc = other_cell.borrow();
            let other_path = uri_to_pathbuf(uri).unwrap_or_default();
            let other_rel = relative_to(project_root, &other_path);
            let other_rel_str = path_to_string(&other_rel);
            // Re-lower to get the decl ident's byte_range. The
            // `ProjectAnalysis` already has the HIR cached on the
            // module entry, but it's keyed on the Uri — we look it up
            // there to avoid double-lowering.
            let other_hir = greycat_analyzer_hir::lower::lower_module(
                &other_doc.text,
                "module",
                &other_doc.lib,
                other_doc.root_node(),
            );
            if (*decl).into_raw() as usize >= other_hir.decls.len() {
                return None;
            }
            let actual_decl = &other_hir.decls[*decl];
            let (rk, dk, name) = decl_summary(&other_hir, actual_decl);
            let name_id = actual_decl.name()?;
            let id = &other_hir.idents[name_id];
            let other_lib = module_stem_from_uri(uri).unwrap_or_else(|| "core".to_string());
            let fqn = format!("{other_lib}::{name}");
            let (line, col) = line_col(&other_doc.text, id.byte_range.start);
            Some(DeclPayload {
                ref_kind: rk,
                decl_kind: dk,
                name,
                fqn,
                decl_file: other_rel_str,
                decl_range: id.byte_range.clone(),
                decl_line: line,
                decl_col: col,
            })
        }
        Definition::Project => None,
    }
}

fn decl_summary(hir: &Hir, decl: &Decl) -> (&'static str, &'static str, String) {
    match decl {
        Decl::Fn(d) => ("fn", "FnDecl", hir.idents[d.name].text.to_string()),
        Decl::Type(d) => ("type", "TypeDecl", hir.idents[d.name].text.to_string()),
        Decl::Enum(d) => ("type", "EnumDecl", hir.idents[d.name].text.to_string()),
        Decl::Var(d) => ("var", "ModuleVar", hir.idents[d.name].text.to_string()),
        Decl::Pragma(p) => ("type", "Pragma", hir.idents[p.name].text.to_string()),
    }
}

fn push_resolution_record(
    out: &mut Vec<Record>,
    file: &str,
    text: &str,
    byte_range: &std::ops::Range<usize>,
    payload: &DeclPayload,
) {
    if byte_range.start > text.len() || byte_range.end > text.len() {
        return;
    }
    let (line, col) = line_col(text, byte_range.start);
    let (end_line, end_col) = line_col(text, byte_range.end);
    // For same-file payloads, fill in line/col now (we have `text`).
    let (decl_line, decl_col) = if payload.decl_line == 0 && payload.decl_col == 0 {
        if payload.decl_file == file {
            line_col(text, payload.decl_range.start)
        } else {
            (payload.decl_line, payload.decl_col)
        }
    } else {
        (payload.decl_line, payload.decl_col)
    };
    let decl_json = format!(
        "{{\"fqn\":{fqn},\"file\":{f},\"range\":[{rs},{re}],\"line\":{l},\"col\":{c}}}",
        fqn = json_string(&payload.fqn),
        f = json_string(&payload.decl_file),
        rs = payload.decl_range.start,
        re = payload.decl_range.end,
        l = decl_line,
        c = decl_col,
    );
    let json = format!(
        "{{\"file\":{f},\"range\":[{rs},{re}],\"line\":{line},\"col\":{col},\"endLine\":{el},\"endCol\":{ec},\"refKind\":{rk},\"declKind\":{dk},\"name\":{nm},\"decl\":{decl}}}",
        f = json_string(file),
        rs = byte_range.start,
        re = byte_range.end,
        el = end_line,
        ec = end_col,
        rk = json_string(payload.ref_kind),
        dk = json_string(payload.decl_kind),
        nm = json_string(&payload.name),
        decl = decl_json,
    );
    out.push(Record {
        file: file.to_string(),
        line,
        col,
        range_start: byte_range.start,
        range_end: byte_range.end,
        json,
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a byte offset to (line:1-based, col:0-based UTF-8 bytes).
fn line_col(text: &str, byte: usize) -> (u32, u32) {
    let upto = byte.min(text.len());
    let mut line: u32 = 1;
    let mut col: u32 = 0;
    for (i, ch) in text.char_indices() {
        if i >= upto {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf8() as u32;
        }
    }
    (line, col)
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[allow(dead_code)]
fn _silence_unused(_: Resolutions) {}
