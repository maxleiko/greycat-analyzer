//! Stdlib ingestion (P2.6).
//!
//! Loads `lib/std/*.gcl` as ordinary HIR modules and registers their
//! declared types and native-bound function signatures into shared
//! [`TypeArena`] / [`TypeRegistry`] / [`NativeRegistry`] structures so
//! the analyzer can resolve `int`, `String`, `Array`, `node`, etc.
//! against real declarations rather than the stub `BUILTIN_TYPES`
//! allowlist the resolver currently pre-seeds.
//!
//! Decision F (ROADMAP §3): runtime-implemented (`native`) functions
//! get a small Rust metadata table — signatures only, no bodies. Their
//! .gcl source captures the signature; this module collects them so
//! call-site type checking works even though there's no body to walk.

use std::collections::{HashMap, HashSet};

use greycat_analyzer_core::lsp_types::Uri;
use greycat_analyzer_hir::Hir;
use greycat_analyzer_hir::arena::Idx;
use greycat_analyzer_hir::types::{Decl, FnDecl, TypeRef as HirTypeRef};
use greycat_analyzer_types::{Primitive, Type, TypeArena, TypeId, TypeKind, TypeRegistry};

/// Cross-module registry of native-bound function signatures. Keyed by
/// canonical name (`<lib>::<module>::<fn>` once we wire fully-qualified
/// resolution; just `<fn>` for now until P2.7 multi-module work).
#[derive(Debug, Default)]
pub struct NativeRegistry {
    pub signatures: HashMap<String, NativeSignature>,
}

#[derive(Debug, Clone)]
pub struct NativeSignature {
    pub params: Vec<TypeId>,
    pub return_ty: TypeId,
}

impl NativeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: impl Into<String>, sig: NativeSignature) {
        self.signatures.insert(name.into(), sig);
    }

    pub fn lookup(&self, name: &str) -> Option<&NativeSignature> {
        self.signatures.get(name)
    }
}

/// Cross-module project context: the shared arena / registry / native
/// table that survives across module ingestion. Distinct from
/// [`crate::analyzer::AnalysisResult`], which is per-module.
#[derive(Debug, Default)]
pub struct ProjectIndex {
    pub types: TypeArena,
    pub registry: TypeRegistry,
    pub natives: NativeRegistry,
    /// Top-level value-position names from every ingested module —
    /// non-native `fn` declarations, top-level `var` declarations.
    /// Lets the resolver answer "is this name known anywhere in the
    /// project?" without needing the cross-module decl pointer (a
    /// later P6.x deliverable).
    pub values: HashSet<String>,
    /// Cross-module decl table (P11.1): name → every `(Uri, Idx<Decl>)`
    /// pair that introduces a top-level decl with this name across the
    /// project. Collisions are kept; disambiguation happens at the use
    /// site via the importing module's lib/include closure (P11.2+).
    /// Pragma decls have no name and are excluded.
    pub decl_locations: HashMap<String, Vec<(Uri, Idx<Decl>)>>,
    /// P13.4 — runtime-exposed names. Keyed by the rename string of
    /// `@expose("renamed")` (or the decl's own name when `@expose` has
    /// no arg) → every site that exposed under that key. Lets lints /
    /// capabilities ask "is this name part of the runtime API?".
    pub exposed: HashMap<String, Vec<ExposureSite>>,
    /// Total number of modules ingested. Useful for "did stdlib actually
    /// load?" smoke checks at the LSP boundary.
    pub modules_ingested: usize,
}

/// P13.4 — a single `@expose`-annotated decl, recorded for the
/// runtime-API surface. `local_name` is the source-level name in the
/// declaring module; `rename` is what `@expose("renamed")` gave it
/// (or `None` when `@expose` was used bare).
#[derive(Debug, Clone)]
pub struct ExposureSite {
    pub uri: Uri,
    pub decl: Idx<Decl>,
    pub local_name: String,
    pub rename: Option<String>,
}

impl ProjectIndex {
    pub fn new() -> Self {
        let mut idx = Self::default();
        seed_builtin_primitives(&mut idx.types);
        seed_builtin_names(&mut idx.types, &mut idx.registry);
        idx
    }

    /// Walk a HIR module's top-level decls and register everything that's
    /// a type-name (type / enum) or a native function, recording each
    /// named decl into [`Self::decl_locations`] keyed by `uri`. Re-entrant:
    /// calling twice with the same `(uri, hir)` is a no-op apart from the
    /// counter — duplicate `(uri, decl_id)` pairs are not appended.
    pub fn ingest(&mut self, uri: &Uri, hir: &Hir) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            let modifiers = match &hir.decls[*decl_id] {
                Decl::Type(td) => {
                    let name = hir.idents[td.name].text.clone();
                    if self.registry.lookup(&name).is_none() {
                        // If the type has generic params, we register a
                        // GenericParam-shaped entry pre-instantiated as
                        // Named(name); P2.4's generic instantiation logic
                        // takes over at use sites.
                        let id = self.types.named(&name);
                        self.registry.register(name.clone(), id);
                    }
                    self.record_decl_location(name, uri, *decl_id);
                    Some(&td.modifiers)
                }
                Decl::Enum(ed) => {
                    let name = hir.idents[ed.name].text.clone();
                    if self.registry.lookup(&name).is_none() {
                        let variants: Vec<String> = ed
                            .fields
                            .iter()
                            .map(|f| hir.idents[hir.enum_fields[*f].name].text.clone())
                            .collect();
                        let id = self.types.alloc(Type {
                            kind: TypeKind::Enum {
                                name: name.clone(),
                                variants,
                            },
                            nullable: false,
                        });
                        self.registry.register(name.clone(), id);
                    }
                    self.record_decl_location(name, uri, *decl_id);
                    Some(&ed.modifiers)
                }
                Decl::Fn(fnd) => {
                    let name = hir.idents[fnd.name].text.clone();
                    if fnd.modifiers.native {
                        let sig = native_signature_for(hir, fnd, &mut self.types);
                        self.natives.register(name.clone(), sig);
                    } else {
                        self.values.insert(name.clone());
                    }
                    self.record_decl_location(name, uri, *decl_id);
                    Some(&fnd.modifiers)
                }
                Decl::Var(vd) => {
                    let name = hir.idents[vd.name].text.clone();
                    self.values.insert(name.clone());
                    self.record_decl_location(name, uri, *decl_id);
                    Some(&vd.modifiers)
                }
                Decl::Pragma(_) => None,
            };
            // P13.4: walk modifiers' annotations for `@expose("name")`
            // and capture the rename target into the project-wide
            // exposed map.
            if let Some(modifiers) = modifiers {
                let local_name = hir.decls[*decl_id]
                    .name()
                    .map(|n| hir.idents[n].text.clone())
                    .unwrap_or_default();
                for ann in &modifiers.annotations {
                    if ann.name != "expose" {
                        continue;
                    }
                    let rename = ann.args.first().cloned();
                    let key = rename.clone().unwrap_or_else(|| local_name.clone());
                    let entries = self.exposed.entry(key).or_default();
                    let already = entries
                        .iter()
                        .any(|s| s.uri == *uri && s.decl == *decl_id && s.rename == rename);
                    if !already {
                        entries.push(ExposureSite {
                            uri: uri.clone(),
                            decl: *decl_id,
                            local_name: local_name.clone(),
                            rename,
                        });
                    }
                }
            }
        }
        self.modules_ingested += 1;
    }

    fn record_decl_location(&mut self, name: String, uri: &Uri, decl_id: Idx<Decl>) {
        let entry = self.decl_locations.entry(name).or_default();
        if !entry.iter().any(|(u, d)| u == uri && *d == decl_id) {
            entry.push((uri.clone(), decl_id));
        }
    }

    /// Cross-module decl lookup (P11.1): every `(Uri, Idx<Decl>)` pair
    /// known under this name. Empty slice when the name is unknown.
    /// Built-in runtime type names (`Array`, `Map`, …) and language
    /// primitives have no `.gcl` decl and so never appear here — use
    /// [`Self::has_name`] to ask the broader "is this name known?"
    /// question.
    pub fn locate_decl(&self, name: &str) -> &[(Uri, Idx<Decl>)] {
        self.decl_locations
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// `true` iff `name` resolves against any name the project knows:
    /// a registered type / enum, a native fn signature, or a top-level
    /// non-native fn / var. Resolver uses this as the post-local-scope
    /// fallback (P6.2).
    pub fn has_name(&self, name: &str) -> bool {
        self.registry.lookup(name).is_some()
            || self.natives.lookup(name).is_some()
            || self.values.contains(name)
    }
}

fn seed_builtin_primitives(arena: &mut TypeArena) {
    for p in [
        Primitive::Bool,
        Primitive::Int,
        Primitive::Float,
        Primitive::Char,
        Primitive::String,
        Primitive::Time,
        Primitive::Duration,
        Primitive::Geo,
    ] {
        let _ = arena.primitive(p);
    }
    let _ = arena.null();
    let _ = arena.any();
    let _ = arena.never();
}

/// Names the analyzer treats as known without an `.gcl` declaration in
/// scope: the GreyCat primitives plus the runtime-implemented types
/// (collections, node tags, function/tuple/field markers). Registering
/// them here is what lets the resolver retire its hard-coded
/// `BUILTIN_TYPES` allowlist (P6.2) — every name a user can write
/// resolves through one path now.
fn seed_builtin_names(arena: &mut TypeArena, registry: &mut TypeRegistry) {
    // Primitives — registered by name so the resolver's project-index
    // fallback finds them. The TypeIds returned here are the same ones
    // `arena.primitive(...)` allocated in seed_builtin_primitives.
    registry.register("bool", arena.primitive(Primitive::Bool));
    registry.register("int", arena.primitive(Primitive::Int));
    registry.register("float", arena.primitive(Primitive::Float));
    registry.register("char", arena.primitive(Primitive::Char));
    registry.register("String", arena.primitive(Primitive::String));
    registry.register("time", arena.primitive(Primitive::Time));
    registry.register("duration", arena.primitive(Primitive::Duration));
    registry.register("geo", arena.primitive(Primitive::Geo));
    registry.register("any", arena.any());
    registry.register("null", arena.null());

    // Runtime-implemented named types — no `.gcl` decl. Drawn from the
    // TS `StdCoreTypes` interface plus the t<n> / t<n>f tuple shapes.
    for &name in BUILTIN_RUNTIME_TYPES {
        let id = arena.named(name);
        registry.register(name, id);
    }
}

/// Type names whose declaration lives in the GreyCat runtime, not in
/// any `.gcl` file. The resolver treats a hit against this list (via
/// the project index registry) as a successful binding. P7.3 refines
/// the subtyping rules these tags participate in.
pub const BUILTIN_RUNTIME_TYPES: &[&str] = &[
    "Array",
    "Map",
    "Set",
    "node",
    "nodeTime",
    "nodeGeo",
    "nodeList",
    "nodeIndex",
    "function",
    "type",
    "tuple",
    "field",
    "t2",
    "t3",
    "t4",
    "t2f",
    "t3f",
    "t4f",
];

fn native_signature_for(hir: &Hir, fnd: &FnDecl, types: &mut TypeArena) -> NativeSignature {
    let params = fnd
        .params
        .iter()
        .map(|p_id| {
            let p = &hir.fn_params[*p_id];
            p.ty.map(|t| lower_type_ref(hir, t, types))
                .unwrap_or_else(|| types.any())
        })
        .collect();
    let return_ty = fnd
        .return_type
        .map(|t| lower_type_ref(hir, t, types))
        .unwrap_or_else(|| types.any());
    NativeSignature { params, return_ty }
}

fn lower_type_ref(
    hir: &Hir,
    idx: greycat_analyzer_hir::arena::Idx<HirTypeRef>,
    types: &mut TypeArena,
) -> TypeId {
    let tr = &hir.type_refs[idx];
    let name = hir.idents[tr.name].text.clone();
    let mut base = match name.as_str() {
        "bool" => types.primitive(Primitive::Bool),
        "int" => types.primitive(Primitive::Int),
        "float" => types.primitive(Primitive::Float),
        "char" => types.primitive(Primitive::Char),
        "String" => types.primitive(Primitive::String),
        "time" => types.primitive(Primitive::Time),
        "duration" => types.primitive(Primitive::Duration),
        "geo" => types.primitive(Primitive::Geo),
        "any" => types.any(),
        "null" => types.null(),
        _ => {
            if !tr.params.is_empty() {
                let args: Vec<TypeId> = tr
                    .params
                    .iter()
                    .map(|p| lower_type_ref(hir, *p, types))
                    .collect();
                types.generic(name, args)
            } else {
                types.named(name)
            }
        }
    };
    if tr.optional {
        base = types.nullable(base);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use greycat_analyzer_hir::lower_module;
    use greycat_analyzer_syntax::parse;
    use std::str::FromStr;

    fn lower(src: &str) -> Hir {
        let tree = parse(src);
        lower_module(src, "stdmod", "std", tree.root_node())
    }

    fn uri(path: &str) -> Uri {
        Uri::from_str(&format!("file://{path}")).unwrap()
    }

    #[test]
    fn ingest_registers_type_decls() {
        let hir = lower(
            r#"
type Person {
    name: String;
    age: int;
}

type Company {
    people: Array<Person>;
}
"#,
        );
        let mut idx = ProjectIndex::new();
        idx.ingest(&uri("/proj/people.gcl"), &hir);
        assert_eq!(idx.modules_ingested, 1);
        assert!(idx.registry.lookup("Person").is_some());
        assert!(idx.registry.lookup("Company").is_some());
    }

    #[test]
    fn ingest_registers_enum_decls() {
        let hir = lower("enum Color { Red, Green, Blue }\n");
        let mut idx = ProjectIndex::new();
        idx.ingest(&uri("/proj/color.gcl"), &hir);
        let id = idx.registry.lookup("Color").expect("Color registered");
        let ty = idx.types.get(id);
        let TypeKind::Enum { variants, .. } = &ty.kind else {
            panic!("expected enum, got {ty:?}");
        };
        assert_eq!(variants, &["Red", "Green", "Blue"]);
    }

    #[test]
    fn ingest_captures_native_signatures() {
        let hir = lower(
            r#"
private native fn read_file(path: String): String;
private native fn now(): time;
"#,
        );
        let mut idx = ProjectIndex::new();
        idx.ingest(&uri("/proj/io.gcl"), &hir);
        let read = idx.natives.lookup("read_file").expect("read_file present");
        assert_eq!(read.params.len(), 1);
        let now = idx.natives.lookup("now").expect("now present");
        assert!(now.params.is_empty());
    }

    #[test]
    fn ingest_is_idempotent_on_repeated_calls() {
        let hir = lower("type T {}\n");
        let u = uri("/proj/t.gcl");
        let mut idx = ProjectIndex::new();
        idx.ingest(&u, &hir);
        let len_after_first = idx.types.len();
        idx.ingest(&u, &hir);
        assert_eq!(
            idx.types.len(),
            len_after_first,
            "duplicate type registrations"
        );
        assert_eq!(idx.modules_ingested, 2);
        // decl_locations is also idempotent — the same (uri, decl_id)
        // pair shouldn't be appended twice.
        assert_eq!(idx.locate_decl("T").len(), 1);
    }

    #[test]
    fn locate_decl_records_uri_and_decl_id() {
        // Acceptance for P11.1: querying the index for a declared type
        // returns the URI of the module that introduced it and a
        // matching `Idx<Decl>`. Synthetic stand-in for `Permission` in
        // `lib/std/runtime.gcl` so the test doesn't depend on `greycat
        // install` having been run.
        let hir = lower("private type Permission {}\n");
        let permission_uri = uri("/proj/lib/std/runtime.gcl");
        let mut idx = ProjectIndex::new();
        idx.ingest(&permission_uri, &hir);

        let hits = idx.locate_decl("Permission");
        assert_eq!(hits.len(), 1, "exactly one Permission decl across project");
        let (found_uri, decl_id) = &hits[0];
        assert_eq!(found_uri, &permission_uri);
        assert!(matches!(&hir.decls[*decl_id], Decl::Type(_)));
    }

    #[test]
    fn locate_decl_keeps_collisions_across_modules() {
        // Same name in two modules should produce two entries — P11.2
        // disambiguates at the use site via the importer's lib/include
        // closure, but the table itself keeps every hit.
        let hir_a = lower("type Helper {}\n");
        let hir_b = lower("type Helper {}\n");
        let mut idx = ProjectIndex::new();
        idx.ingest(&uri("/proj/a.gcl"), &hir_a);
        idx.ingest(&uri("/proj/b.gcl"), &hir_b);
        let hits = idx.locate_decl("Helper");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, uri("/proj/a.gcl"));
        assert_eq!(hits[1].0, uri("/proj/b.gcl"));
    }

    #[test]
    fn ingest_captures_expose_rename_into_exposed_map() {
        // P13.4: `@expose("renamed")` keys into ProjectIndex::exposed by
        // the renamed string; bare `@expose` keys by the decl's local
        // name.
        let hir = lower(
            r#"
@expose("public_alpha")
fn alpha() {}

@expose
fn beta() {}

@library("std", "1")
fn ignored() {}
"#,
        );
        let u = uri("/proj/api.gcl");
        let mut idx = ProjectIndex::new();
        idx.ingest(&u, &hir);

        let alpha_hits = idx.exposed.get("public_alpha").expect("public_alpha");
        assert_eq!(alpha_hits.len(), 1);
        assert_eq!(alpha_hits[0].rename.as_deref(), Some("public_alpha"));
        assert_eq!(alpha_hits[0].local_name, "alpha");

        let beta_hits = idx.exposed.get("beta").expect("beta");
        assert_eq!(beta_hits.len(), 1);
        assert_eq!(beta_hits[0].rename, None);

        assert!(
            !idx.exposed.contains_key("ignored"),
            "@library annotation shouldn't add to exposed map: {:?}",
            idx.exposed.keys().collect::<Vec<_>>(),
        );
    }

    #[test]
    fn locate_decl_records_fns_and_top_vars() {
        let hir = lower(
            r#"
fn helper(): int { return 1; }
var TOP: int = 1;
"#,
        );
        let u = uri("/proj/m.gcl");
        let mut idx = ProjectIndex::new();
        idx.ingest(&u, &hir);
        assert_eq!(idx.locate_decl("helper").len(), 1);
        assert_eq!(idx.locate_decl("TOP").len(), 1);
        assert!(idx.locate_decl("missing").is_empty());
    }
}
