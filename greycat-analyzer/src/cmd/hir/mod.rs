//! `greycat-analyzer hir <target> [--json]` — full HIR view of the
//! analyzed project.
//!
//! Loads the project entrypoint (project.gcl or single .gcl file
//! walking up to its project root), runs `ProjectAnalysis::analyze`,
//! then projects every module into a borrow-only view tree (see
//! [`view`]). Default output is Rust `Debug` pretty-printed; `--json`
//! switches to `serde_json::to_string_pretty`.
//!
//! Intended as a debugging surface — not a parity oracle (that's
//! `dump-types` / `dump-resolutions`). The shape here mirrors what an
//! IDE consumer would see when walking the analyzed project: per
//! module, every decl with its resolved types and provenance.

pub mod view;

use std::borrow::Cow;
use std::cell::Ref;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use greycat_analyzer_analysis::analyzer::AnalysisResult;
use greycat_analyzer_analysis::display_fqn;
use greycat_analyzer_analysis::index::ProjectIndex;
use greycat_analyzer_analysis::project::{DeclRegistry, ModuleAnalysis, ProjectAnalysis};
use greycat_analyzer_analysis::resolver::Definition;
use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_core::resolver::FsContext;
use greycat_analyzer_core::{
    Document, ItemId, SourceManager, Symbol, SymbolTable, TypeArena, TypeId, TypeKind,
};
use greycat_analyzer_hir::Hir as HirArenas;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, FnDecl, TypeDecl};

use crate::utils::AnyError;

use view::{
    AnnotationView, AttrView, EnumFieldView, EnumView, ExtendsLink, FnView, ModifiersView, Module,
    Monomorphization, ParamView, PragmaView, Project, ResolutionView, TypeView, VarView,
};

#[derive(clap::Parser)]
#[clap(about = "Print the full HIR view of a GreyCat project (debug tool).\n\
            Default output is Rust Debug pretty-printed; pass --json for\n\
            machine-readable output.")]
pub struct HirCmd {
    #[clap(help = "Path to a project.gcl entrypoint, a project directory \
                containing project.gcl, or a single .gcl file (walks up \
                to its enclosing project root). When omitted, looks for \
                `project.gcl` in the current working directory.")]
    target: Option<PathBuf>,
    #[clap(long, help = "Emit JSON instead of Rust Debug.")]
    json: bool,
}

impl HirCmd {
    pub fn run(self) -> Result<ExitCode, AnyError> {
        env_logger::init();
        // When omitted, default to the current working directory — its
        // `project.gcl` becomes the entrypoint (mirrors `lint` / `fmt`).
        let target = match self.target {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        let canonical = target.canonicalize()?;
        let (project_root, _single_file) = resolve_project(&canonical)?;
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

        let arena = analysis.arena();
        let symbols = analysis.symbols();
        let registry = analysis.decl_registry();
        let index = &analysis.index;

        // Deterministic module order (by URI string) so output is
        // stable across runs.
        let mut ordered: Vec<&Uri> = analysis.iter().map(|(u, _)| u).collect();
        ordered.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        // Per-module side buffers — borrows in the view tree point
        // into these long-lived owned strings.
        let module_uri_strs: Vec<String> = ordered.iter().map(|u| u.as_str().to_string()).collect();
        let module_rel_paths: Vec<String> = ordered
            .iter()
            .map(|u| rel_to_project(&project_root, u))
            .collect();

        let module_docs: Vec<Ref<'_, Document>> = ordered
            .iter()
            .map(|uri| {
                mgr.get(uri)
                    .expect("every analyzed module is in the source manager")
                    .borrow()
            })
            .collect();

        let module_analyses: Vec<&ModuleAnalysis> = ordered
            .iter()
            .map(|uri| {
                analysis
                    .iter()
                    .find(|(u, _)| u == uri)
                    .map(|(_, m)| m)
                    .expect("module present")
            })
            .collect();

        // Pre-compute every FQN string the view needs. Buffers outlive
        // the view borrows because they're owned by this run() frame.
        let module_buffers: Vec<ModuleFqnBuffers> = ordered
            .iter()
            .enumerate()
            .map(|(i, _)| {
                build_module_buffers(
                    &module_docs[i],
                    module_analyses[i],
                    arena,
                    symbols,
                    registry,
                    index,
                )
            })
            .collect();

        let monos = collect_monomorphizations(arena, registry, symbols, index);
        let root_str = project_root.to_string_lossy().replace('\\', "/");

        let module_views: Vec<Module<'_>> = (0..ordered.len())
            .map(|i| {
                build_module_view(
                    &module_uri_strs[i],
                    &module_rel_paths[i],
                    &module_docs[i],
                    module_analyses[i],
                    arena,
                    symbols,
                    registry,
                    index,
                    &module_buffers[i],
                )
            })
            .collect();

        let project = Project {
            root: &root_str,
            modules: module_views,
            monomorphizations: monos
                .iter()
                .map(|m| Monomorphization {
                    display: Cow::Borrowed(m.display.as_str()),
                    args: m.args.iter().map(|a| Cow::Borrowed(a.as_str())).collect(),
                })
                .collect(),
        };

        if self.json {
            let s = serde_json::to_string_pretty(&project)?;
            println!("{s}");
        } else {
            println!("{project:#?}");
        }
        Ok(ExitCode::SUCCESS)
    }
}

// ---------------------------------------------------------------------------
// Project resolution (same shape as dump_types::resolve_project)
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

fn rel_to_project(project_root: &Path, uri: &Uri) -> String {
    let s = uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    let path = PathBuf::from(stripped);
    let rel = relative_to(project_root, &path);
    rel.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Per-module FQN buffers (the only owned strings in the project view).
// Cow<'a, str>::Borrowed slots in the view tree borrow from these.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ModuleFqnBuffers {
    type_fqns: BTreeMap<u32, String>,                 // decl_idx_raw
    attr_fqns: BTreeMap<(u32, u32), String>,          // (type_decl, attr_idx)
    fn_return_fqns: BTreeMap<(u32, u32), String>,     // (owner_decl, fn_decl)
    fn_param_fqns: BTreeMap<(u32, u32, u32), String>, // (owner, fn, pos)
    var_fqns: BTreeMap<u32, String>,                  // decl_idx_raw
    extends_fqns: BTreeMap<u32, String>,              // type_decl_idx → instantiated parent
    resolution_fqns: BTreeMap<u32, String>,           // ident_idx_raw
}

fn home_lib_for(index: &ProjectIndex, name: &str) -> Option<String> {
    let sym = index.symbols.lookup(name)?;
    let locs = index.locate_decl(sym);
    locs.first().and_then(|d| {
        let s = d.uri.as_str();
        let stripped = s.strip_prefix("file://").unwrap_or(s);
        let last = stripped.rsplit(['/', '\\']).next()?;
        let stem = last.strip_suffix(".gcl").unwrap_or(last);
        if stem.is_empty() {
            None
        } else {
            Some(stem.to_string())
        }
    })
}

fn fqn_for(
    arena: &TypeArena,
    registry: &DeclRegistry,
    symbols: &SymbolTable,
    index: &ProjectIndex,
    ty: TypeId,
) -> String {
    let home = |n: &str| home_lib_for(index, n);
    display_fqn(arena, registry, symbols, ty, &home)
}

fn build_module_buffers(
    doc: &Document,
    module: &ModuleAnalysis,
    arena: &TypeArena,
    symbols: &SymbolTable,
    registry: &DeclRegistry,
    index: &ProjectIndex,
) -> ModuleFqnBuffers {
    let mut buf = ModuleFqnBuffers::default();
    let hir = &module.hir;
    let analysis = &module.analysis;

    let Some(top) = hir.module.as_ref() else {
        return buf;
    };

    for &decl_idx in top.decls.iter() {
        let decl_raw = decl_idx.into_raw();
        match &hir.decls[decl_idx] {
            Decl::Type(td) => {
                let name = hir.idents[td.name].symbol;
                // Top-level type FQN.
                if let Some(handle) = index.resolve_item(registry, None, name) {
                    let mut tmp_arena = arena.clone();
                    let id = tmp_arena.alloc_type(handle);
                    let s = fqn_for(&tmp_arena, registry, symbols, index, id);
                    buf.type_fqns.insert(decl_raw, s);
                }
                // Extends chain — instantiated parent shape.
                if let Some(item) = index.resolve_item(registry, None, name)
                    && let Some(members) = index.type_members.get(&item)
                    && let Some(parent_ty) = members.supertype_ty
                {
                    let s = fqn_for(arena, registry, symbols, index, parent_ty);
                    buf.extends_fqns.insert(decl_raw, s);
                }
                // Per-attr types.
                for &attr_idx in td.attrs.iter() {
                    let attr = &hir.type_attrs[attr_idx];
                    let attr_name = &symbols[hir.idents[attr.name].symbol];
                    let attr_ty = pre_lowered_attr_ty(registry, index, name, attr_name)
                        .or_else(|| attr.init.and_then(|e| analysis.expr_types.get(&e).copied()));
                    if let Some(ty) = attr_ty {
                        let s = fqn_for(arena, registry, symbols, index, ty);
                        buf.attr_fqns.insert((decl_raw, attr_idx.into_raw()), s);
                    }
                }
                // Per-method signatures.
                for &m in td.methods.iter() {
                    let Decl::Fn(fnd) = &hir.decls[m] else {
                        continue;
                    };
                    record_fn_signature(
                        &mut buf,
                        hir,
                        analysis,
                        arena,
                        registry,
                        symbols,
                        index,
                        decl_raw,
                        m.into_raw(),
                        fnd,
                    );
                }
            }
            Decl::Fn(fnd) => {
                record_fn_signature(
                    &mut buf, hir, analysis, arena, registry, symbols, index, decl_raw, decl_raw,
                    fnd,
                );
            }
            Decl::Var(vd) => {
                let ty = analysis
                    .def_types
                    .get(&vd.name)
                    .copied()
                    .or_else(|| vd.init.and_then(|e| analysis.expr_types.get(&e).copied()));
                if let Some(ty) = ty {
                    let s = fqn_for(arena, registry, symbols, index, ty);
                    buf.var_fqns.insert(decl_raw, s);
                }
            }
            Decl::Enum(_) | Decl::Pragma(_) => {}
        }
    }

    // Resolutions — FQN for each ident-use that binds to a named decl.
    let module_stem = doc.name();
    for (ident_idx, def) in module.resolutions.uses.iter() {
        let ident = &hir.idents[*ident_idx];
        let name = &symbols[ident.symbol];
        let fqn = match def {
            Definition::Decl(_) => Some(format!("{module_stem}::{name}")),
            Definition::ProjectDecl { uri, .. } => {
                let s = uri.as_str();
                let stripped = s.strip_prefix("file://").unwrap_or(s);
                let last = stripped.rsplit(['/', '\\']).next();
                let stem = last
                    .map(|l| l.strip_suffix(".gcl").unwrap_or(l))
                    .unwrap_or("core");
                Some(format!("{stem}::{name}"))
            }
            Definition::Project => {
                let home = home_lib_for(index, name).unwrap_or_else(|| "core".to_string());
                Some(format!("{home}::{name}"))
            }
            _ => None,
        };
        if let Some(s) = fqn {
            buf.resolution_fqns.insert(ident_idx.into_raw(), s);
        }
    }

    buf
}

#[allow(clippy::too_many_arguments)]
fn record_fn_signature(
    buf: &mut ModuleFqnBuffers,
    hir: &HirArenas,
    analysis: &AnalysisResult,
    arena: &TypeArena,
    registry: &DeclRegistry,
    symbols: &SymbolTable,
    index: &ProjectIndex,
    owner_decl_raw: u32,
    fn_decl_raw: u32,
    fnd: &FnDecl,
) {
    if let Some(ty) = analysis.def_types.get(&fnd.name).copied() {
        let s = fqn_for(arena, registry, symbols, index, ty);
        buf.fn_return_fqns.insert((owner_decl_raw, fn_decl_raw), s);
    }
    for (pos, &param_idx) in fnd.params.iter().enumerate() {
        let param = &hir.fn_params[param_idx];
        if let Some(ty) = analysis.def_types.get(&param.name).copied() {
            let s = fqn_for(arena, registry, symbols, index, ty);
            buf.fn_param_fqns
                .insert((owner_decl_raw, fn_decl_raw, pos as u32), s);
        }
    }
}

/// Pre-lowered attr type from `ProjectIndex::type_members[…].attr_types`.
/// Returns `None` when signature lowering didn't reach this attr.
fn pre_lowered_attr_ty(
    registry: &DeclRegistry,
    index: &ProjectIndex,
    type_name: Symbol,
    attr_name: &str,
) -> Option<TypeId> {
    let item = index.resolve_item(registry, None, type_name)?;
    let members = index.type_members.get(&item)?;
    let attr_sym = index.symbols.lookup(attr_name)?;
    members.attr_types.get(&attr_sym).copied()
}

// ---------------------------------------------------------------------------
// Module view assembly
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_module_view<'a>(
    uri_str: &'a str,
    rel_path: &'a str,
    doc: &'a Document,
    module: &'a ModuleAnalysis,
    arena: &TypeArena,
    symbols: &'a SymbolTable,
    registry: &DeclRegistry,
    index: &'a ProjectIndex,
    buf: &'a ModuleFqnBuffers,
) -> Module<'a> {
    let hir = &module.hir;
    let text = doc.text.as_str();
    let mod_name = doc.name();

    let mut types: Vec<TypeView<'a>> = Vec::new();
    let mut fns: Vec<FnView<'a>> = Vec::new();
    let mut enums: Vec<EnumView<'a>> = Vec::new();
    let mut vars: Vec<VarView<'a>> = Vec::new();
    let mut pragmas: Vec<PragmaView<'a>> = Vec::new();

    let Some(top) = hir.module.as_ref() else {
        return Module {
            name: mod_name,
            lib: module.lib.as_str(),
            uri: uri_str,
            rel_path,
            types,
            fns,
            enums,
            vars,
            pragmas,
            resolutions: Vec::new(),
        };
    };

    for &decl_idx in top.decls.iter() {
        let decl_raw = decl_idx.into_raw();
        match &hir.decls[decl_idx] {
            Decl::Type(td) => {
                let name = hir.idents[td.name].symbol;
                let generics = td
                    .generics
                    .iter()
                    .map(|g| &symbols[hir.idents[*g].symbol])
                    .collect();
                let extends_chain = build_extends_chain(td, hir, symbols, registry, index, buf);
                let attrs = td
                    .attrs
                    .iter()
                    .map(|aix| {
                        let a = &hir.type_attrs[*aix];
                        let aname: &'a str = &symbols[hir.idents[a.name].symbol];
                        let fqn = buf
                            .attr_fqns
                            .get(&(decl_raw, aix.into_raw()))
                            .map(|s| Cow::Borrowed(s.as_str()));
                        AttrView {
                            name: aname,
                            id: aix.into_raw(),
                            modifiers: build_modifiers(symbols, &a.modifiers),
                            ty: fqn,
                            has_init: a.init.is_some(),
                            doc: a.doc.as_deref(),
                        }
                    })
                    .collect();
                let methods = td
                    .methods
                    .iter()
                    .map(|m| match &hir.decls[*m] {
                        Decl::Fn(fnd) => build_fn_view(fnd, *m, decl_raw, hir, symbols, buf),
                        _ => placeholder_fn(*m),
                    })
                    .collect();
                let item = index.resolve_item(registry, None, name);
                types.push(TypeView {
                    name: &symbols[name],
                    id: decl_raw,
                    type_id: item.and_then(|i| find_type_id_for_item(arena, i)),
                    modifiers: build_modifiers(symbols, &td.modifiers),
                    generics,
                    extends_chain,
                    attrs,
                    methods,
                    doc: td.doc.as_deref(),
                });
            }
            Decl::Fn(fnd) => {
                fns.push(build_fn_view(fnd, decl_idx, decl_raw, hir, symbols, buf));
            }
            Decl::Enum(ed) => {
                let name: &'a str = &symbols[hir.idents[ed.name].symbol];
                let fields = ed
                    .fields
                    .iter()
                    .map(|fix| {
                        let f = &hir.enum_fields[*fix];
                        EnumFieldView {
                            name: &symbols[hir.idents[f.name].symbol],
                            has_value: f.value.is_some(),
                        }
                    })
                    .collect();
                enums.push(EnumView {
                    name,
                    id: decl_raw,
                    modifiers: build_modifiers(symbols, &ed.modifiers),
                    fields,
                    doc: ed.doc.as_deref(),
                });
            }
            Decl::Var(vd) => {
                let name: &'a str = &symbols[hir.idents[vd.name].symbol];
                let ty = buf
                    .var_fqns
                    .get(&decl_raw)
                    .map(|s| Cow::Borrowed(s.as_str()));
                let init_slice = vd.init.map(|e| {
                    let r = hir.exprs[e].byte_range();
                    safe_slice(text, r.start, r.end)
                });
                vars.push(VarView {
                    name,
                    id: decl_raw,
                    modifiers: build_modifiers(symbols, &vd.modifiers),
                    ty,
                    initializer: init_slice,
                });
            }
            Decl::Pragma(p) => {
                let name: &'a str = &symbols[hir.idents[p.name].symbol];
                let args = p
                    .args
                    .iter()
                    .map(|e| {
                        let r = hir.exprs[*e].byte_range();
                        safe_slice(text, r.start, r.end)
                    })
                    .collect();
                pragmas.push(PragmaView { name, args });
            }
        }
    }

    let mut resolutions: Vec<ResolutionView<'a>> = module
        .resolutions
        .uses
        .iter()
        .map(|(ident_idx, def)| {
            let ident = &hir.idents[*ident_idx];
            let source = safe_slice(text, ident.byte_range.start, ident.byte_range.end);
            let kind: &'static str = match def {
                Definition::Decl(_) => "decl",
                Definition::Local(_) => "local",
                Definition::Param(_) => "param",
                Definition::Generic(_) => "generic",
                Definition::ProjectDecl { .. } => "project-decl",
                Definition::Project => "project",
            };
            let binds_to = buf
                .resolution_fqns
                .get(&ident_idx.into_raw())
                .map(|s| Cow::Borrowed(s.as_str()));
            ResolutionView {
                source,
                byte_range: ident.byte_range.clone(),
                binds_to,
                kind,
            }
        })
        .collect();
    resolutions.sort_by_key(|r| (r.byte_range.start, r.byte_range.end));

    Module {
        name: mod_name,
        lib: module.lib.as_str(),
        uri: uri_str,
        rel_path,
        types,
        fns,
        enums,
        vars,
        pragmas,
        resolutions,
    }
}

fn build_extends_chain<'a>(
    td: &TypeDecl,
    hir: &'a HirArenas,
    symbols: &'a SymbolTable,
    registry: &DeclRegistry,
    index: &'a ProjectIndex,
    buf: &'a ModuleFqnBuffers,
) -> Vec<ExtendsLink<'a>> {
    let mut out = Vec::new();
    let Some(parent_ref) = td.supertype else {
        return out;
    };
    let parent_name: &'a str = &symbols[hir.idents[hir.type_refs[parent_ref].name].symbol];
    let parent_lib = name_to_lib_borrow(parent_name, index, symbols);

    // Look the instantiated-parent display up by matching on the
    // parent's bare name inside the buffer's cached strings. We don't
    // have the sub's enclosing `Idx<Decl>` here (the TypeDecl carries
    // only the name ident), so we fall back to a content match — fine
    // for a debug dump and avoids threading another key through.
    let sub_name = hir.idents[td.name].symbol;
    let sub_item = index.resolve_item(registry, None, sub_name);
    let first_instantiated: Option<Cow<'a, str>> = buf
        .extends_fqns
        .values()
        .find(|s| {
            s.split("::")
                .nth(1)
                .map(|n| n.split('<').next().unwrap_or(n) == parent_name)
                .unwrap_or(false)
        })
        .map(|s| Cow::Borrowed(s.as_str()));

    out.push(ExtendsLink {
        name: parent_name,
        lib: parent_lib,
        instantiated: first_instantiated,
    });

    // Ancestors — walk through ProjectIndex::type_members, starting
    // one hop past the direct parent (already pushed above).
    let direct_parent = sub_item.and_then(|item| index.type_members.get(&item)?.supertype);
    let mut cur = direct_parent.and_then(|item| index.type_members.get(&item)?.supertype);
    let mut depth = 1usize;
    while let Some(item) = cur {
        if depth >= ProjectIndex::MAX_INHERITANCE_DEPTH {
            break;
        }
        let aname: &'a str = symbols.resolve(&item.name);
        let lib = Some(symbols.resolve(&item.module));
        out.push(ExtendsLink {
            name: aname,
            lib,
            instantiated: None,
        });
        cur = index.type_members.get(&item).and_then(|m| m.supertype);
        depth += 1;
    }
    out
}

fn name_to_lib_borrow<'a>(
    name: &str,
    index: &ProjectIndex,
    symbols: &'a SymbolTable,
) -> Option<&'a str> {
    let sym = index.symbols.lookup(name)?;
    let locs = index.locate_decl(sym);
    let d = locs.first()?;
    let s = d.uri.as_str();
    let stripped = s.strip_prefix("file://").unwrap_or(s);
    let last = stripped.rsplit(['/', '\\']).next()?;
    let stem = last.strip_suffix(".gcl").unwrap_or(last);
    // Lookup the stem via the project SymbolTable — it has a borrow
    // tied to the table's `Arc<Rodeo>` lifetime (which outlives the
    // view).
    let stem_sym = symbols.lookup(stem)?;
    Some(symbols.resolve(&stem_sym))
}

fn build_fn_view<'a>(
    fnd: &'a FnDecl,
    fn_decl_idx: Idx<Decl>,
    owner_decl_raw: u32,
    hir: &'a HirArenas,
    symbols: &'a SymbolTable,
    buf: &'a ModuleFqnBuffers,
) -> FnView<'a> {
    let name: &'a str = &symbols[hir.idents[fnd.name].symbol];
    let generics = fnd
        .generics
        .iter()
        .map(|g| &symbols[hir.idents[*g].symbol])
        .collect();
    let fn_decl_raw = fn_decl_idx.into_raw();
    let params = fnd
        .params
        .iter()
        .enumerate()
        .map(|(pos, &pidx)| {
            let p = &hir.fn_params[pidx];
            let pname: &'a str = &symbols[hir.idents[p.name].symbol];
            let ty = buf
                .fn_param_fqns
                .get(&(owner_decl_raw, fn_decl_raw, pos as u32))
                .map(|s| Cow::Borrowed(s.as_str()));
            ParamView { name: pname, ty }
        })
        .collect();
    let return_ty = buf
        .fn_return_fqns
        .get(&(owner_decl_raw, fn_decl_raw))
        .map(|s| Cow::Borrowed(s.as_str()));
    FnView {
        name,
        id: fn_decl_raw,
        modifiers: build_modifiers(symbols, &fnd.modifiers),
        generics,
        params,
        return_ty,
        has_body: fnd.body.is_some(),
        doc: fnd.doc.as_deref(),
    }
}

fn placeholder_fn<'a>(decl_idx: Idx<Decl>) -> FnView<'a> {
    FnView {
        name: "<non-fn method slot>",
        id: decl_idx.into_raw(),
        modifiers: ModifiersView {
            private: false,
            static_: false,
            abstract_: false,
            native: false,
            annotations: Vec::new(),
        },
        generics: Vec::new(),
        params: Vec::new(),
        return_ty: None,
        has_body: false,
        doc: None,
    }
}

fn build_modifiers<'a>(
    symbols: &'a SymbolTable,
    m: &'a greycat_analyzer_hir::types::Modifiers,
) -> ModifiersView<'a> {
    ModifiersView {
        private: m.private,
        static_: m.static_,
        abstract_: m.abstract_,
        native: m.native,
        annotations: m
            .annotations
            .iter()
            .map(|a| AnnotationView {
                name: &symbols[a.name.symbol],
                args: a.args.iter().map(|arg| render_arg(symbols, arg)).collect(),
            })
            .collect(),
    }
}

fn render_arg(symbols: &SymbolTable, arg: &greycat_analyzer_hir::types::AnnotationArg) -> String {
    use greycat_analyzer_hir::types::AnnotationArgKind as A;
    match &arg.kind {
        A::Int(v) => v.to_string(),
        A::Float(v) => v.to_string(),
        A::Bool(b) => b.to_string(),
        A::Char(c) => format!("'{c}'"),
        A::String(s) => format!("\"{}\"", &symbols[*s]),
        A::Duration(v) => format!("{v}us"),
        A::Time(v) | A::Iso8601(v) => format!("{v}time"),
        A::Null => "null".to_string(),
        A::Path { chain } => chain
            .iter()
            .map(|s| symbols[*s].to_string())
            .collect::<Vec<_>>()
            .join("::"),
        A::Invalid => "<invalid>".to_string(),
    }
}

fn safe_slice(text: &str, start: usize, end: usize) -> &str {
    if start > text.len() || end > text.len() || start > end {
        return "";
    }
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return "";
    }
    &text[start..end]
}

/// Linear-scan the arena for the canonical (non-nullable)
/// `TypeKind::Type(item)` entry.
fn find_type_id_for_item(arena: &TypeArena, item: ItemId) -> Option<u32> {
    arena.items.iter().enumerate().find_map(|(i, ty)| {
        if matches!(&ty.kind, TypeKind::Type(d) if *d == item) && !ty.nullable {
            Some(i as u32)
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Monomorphization collection
// ---------------------------------------------------------------------------

struct MonoEntry {
    display: String,
    args: Vec<String>,
}

fn collect_monomorphizations(
    arena: &TypeArena,
    registry: &DeclRegistry,
    symbols: &SymbolTable,
    index: &ProjectIndex,
) -> Vec<MonoEntry> {
    use std::collections::BTreeSet;
    let home = |n: &str| home_lib_for(index, n);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<MonoEntry> = Vec::new();
    for (i, ty) in arena.items.iter().enumerate() {
        if let TypeKind::Generic { decl, args } = &ty.kind {
            let handle = index.resolve_item(registry, None, decl.name);
            if handle != Some(*decl) {
                continue;
            }
            let id = TypeId::from_raw(i as u32);
            let display = display_fqn(arena, registry, symbols, id, &home);
            if !seen.insert(display.clone()) {
                continue;
            }
            let arg_strs: Vec<String> = args
                .iter()
                .map(|a| display_fqn(arena, registry, symbols, *a, &home))
                .collect();
            out.push(MonoEntry {
                display,
                args: arg_strs,
            });
        }
    }
    out
}
