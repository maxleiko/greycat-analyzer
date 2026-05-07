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

use std::collections::HashMap;

use greycat_analyzer_hir::Hir;
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
    /// Total number of modules ingested. Useful for "did stdlib actually
    /// load?" smoke checks at the LSP boundary.
    pub modules_ingested: usize,
}

impl ProjectIndex {
    pub fn new() -> Self {
        let mut idx = Self::default();
        seed_builtin_primitives(&mut idx.types);
        idx
    }

    /// Walk a HIR module's top-level decls and register everything that's
    /// a type-name (type / enum) or a native function. Re-entrant: calling
    /// twice with the same module is a no-op apart from the counter.
    pub fn ingest(&mut self, hir: &Hir) {
        let Some(module) = hir.module.as_ref() else {
            return;
        };
        for decl_id in &module.decls {
            match &hir.decls[*decl_id] {
                Decl::Type(td) => {
                    let name = hir.idents[td.name].text.clone();
                    if self.registry.lookup(&name).is_none() {
                        // If the type has generic params, we register a
                        // GenericParam-shaped entry pre-instantiated as
                        // Named(name); P2.4's generic instantiation logic
                        // takes over at use sites.
                        let id = self.types.named(&name);
                        self.registry.register(name, id);
                    }
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
                        self.registry.register(name, id);
                    }
                }
                Decl::Fn(fnd) => {
                    if fnd.modifiers.native {
                        let sig = native_signature_for(hir, fnd, &mut self.types);
                        let name = hir.idents[fnd.name].text.clone();
                        self.natives.register(name, sig);
                    }
                }
                Decl::Var(_) | Decl::Pragma(_) => {}
            }
        }
        self.modules_ingested += 1;
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

fn native_signature_for(hir: &Hir, fnd: &FnDecl, types: &mut TypeArena) -> NativeSignature {
    let params = fnd
        .params
        .iter()
        .map(|p_id| {
            let p = &hir.fn_params[*p_id];
            p.ty
                .map(|t| lower_type_ref(hir, t, types))
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

    fn lower(src: &str) -> Hir {
        let tree = parse(src);
        lower_module(src, "stdmod", "std", tree.root_node())
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
        idx.ingest(&hir);
        assert_eq!(idx.modules_ingested, 1);
        assert!(idx.registry.lookup("Person").is_some());
        assert!(idx.registry.lookup("Company").is_some());
    }

    #[test]
    fn ingest_registers_enum_decls() {
        let hir = lower("enum Color { Red, Green, Blue }\n");
        let mut idx = ProjectIndex::new();
        idx.ingest(&hir);
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
        idx.ingest(&hir);
        let read = idx.natives.lookup("read_file").expect("read_file present");
        assert_eq!(read.params.len(), 1);
        let now = idx.natives.lookup("now").expect("now present");
        assert!(now.params.is_empty());
    }

    #[test]
    fn ingest_is_idempotent_on_repeated_calls() {
        let hir = lower("type T {}\n");
        let mut idx = ProjectIndex::new();
        idx.ingest(&hir);
        let len_after_first = idx.types.len();
        idx.ingest(&hir);
        assert_eq!(idx.types.len(), len_after_first, "duplicate type registrations");
        assert_eq!(idx.modules_ingested, 2);
    }
}
