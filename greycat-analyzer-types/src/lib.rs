//! Type system for greycat — foundation port (P2.4).
//!
//! Ports the core of `packages/lang/src/analysis/types.ts` (~2,811 LoC of
//! TS). This crate is the foundation P2.5 (analyzer) builds on; it owns
//! the `Type` enum, type interning, and subtyping rules.
//!
//! What's here:
//! - [`Type`]: the central enum (primitives, named, generic, lambda, etc.)
//! - [`TypeId`]: a `Copy` handle into the [`TypeArena`].
//! - Primitive type ids (`null_t()`, `int_t()`, ...) for cheap comparisons.
//! - [`TypeRegistry`]: holds per-module declared types so Named lookups
//!   work without walking the HIR every time.
//! - Subtyping (`is_assignable_to`) covering the cases the analyzer needs
//!   in P2.5: primitive widening, null-into-nullable, generic invariance,
//!   any/never, lambda variance.
//!
//! What's deferred:
//! - Full TS subtyping rules around node types and runtime tagging.
//! - Variance for user-declared generics (TS treats them invariantly).
//! - Inference table / unification beyond simple substitution.
//!
//! Decision B: single typed AST + type arena (no separate hir-def/hir-ty
//! split). Inference table is a thin map from `Idx<Expr>` to `TypeId`
//! and lives in the analyzer crate, not here.

use std::collections::HashMap;

/// A handle into a [`TypeArena`]. Cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl TypeId {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// The central type representation.
///
/// The TS reference uses a class hierarchy with `nullable` flags per type
/// instance; we mirror that with a top-level `nullable` field on every
/// variant via the wrapping [`Type`] struct.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Type {
    pub kind: TypeKind,
    /// `true` iff this type allows `null` as a value (the `T?` syntax).
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeKind {
    /// `null`-only type. Convertible to any nullable.
    Null,
    /// `any` — top type. Anything is assignable to it.
    Any,
    /// `never` — bottom type. Used for unreachable code.
    Never,
    /// Named primitive — `int`, `float`, `String`, `bool`, `char`,
    /// `time`, `duration`, `geo`. Carries the canonical name.
    Primitive(Primitive),
    /// Named user / stdlib type, identified by its fully-qualified name
    /// (`<lib>::<module>::<TypeName>` or just `<TypeName>` until we wire
    /// fully-qualified resolution).
    Named { name: String },
    /// Generic type instantiation — `Array<int>`, `Map<String, int>`, etc.
    Generic { name: String, args: Vec<TypeId> },
    /// Generic type *parameter* — the `T` inside a `fn<T>(x: T)` body.
    GenericParam { name: String, owner: GenericOwner },
    /// Function / lambda type.
    Lambda(LambdaType),
    /// Tuple — `t2`, `t3`, `t4` plus their float variants.
    Tuple { elements: Vec<TypeId> },
    /// Anonymous object literal type — `{ a: int, b: String }`.
    Anonymous { fields: Vec<(String, TypeId)> },
    /// Enum type.
    Enum { name: String, variants: Vec<String> },
    /// Union of two-or-more alternatives. Construction normalizes:
    /// `T | T = T`, `T | null = nullable(T)`.
    Union { alts: Vec<TypeId> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    Bool,
    Int,
    Float,
    Char,
    String,
    Time,
    Duration,
    Geo,
}

impl Primitive {
    pub fn name(self) -> &'static str {
        match self {
            Primitive::Bool => "bool",
            Primitive::Int => "int",
            Primitive::Float => "float",
            Primitive::Char => "char",
            Primitive::String => "String",
            Primitive::Time => "time",
            Primitive::Duration => "duration",
            Primitive::Geo => "geo",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LambdaType {
    pub params: Vec<TypeId>,
    pub ret: TypeId,
}

/// Where a generic parameter was declared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GenericOwner {
    /// `fn<T>(...)`.
    Function(String),
    /// `type Foo<T> {...}`.
    Type(String),
}

// =============================================================================
// Arena
// =============================================================================

/// Append-only interning arena for `Type`. Two equal `Type` values get
/// the same [`TypeId`]; comparing for equality is then just an integer
/// comparison.
#[derive(Debug, Default)]
pub struct TypeArena {
    items: Vec<Type>,
    intern: HashMap<Type, TypeId>,
}

impl TypeArena {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&mut self, ty: Type) -> TypeId {
        if let Some(&id) = self.intern.get(&ty) {
            return id;
        }
        let id = TypeId(self.items.len() as u32);
        self.items.push(ty.clone());
        self.intern.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &Type {
        &self.items[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Make a copy of `id` with `nullable = true`. Idempotent.
    pub fn nullable(&mut self, id: TypeId) -> TypeId {
        let mut ty = self.get(id).clone();
        if ty.nullable {
            return id;
        }
        ty.nullable = true;
        self.alloc(ty)
    }

    pub fn primitive(&mut self, p: Primitive) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Primitive(p),
            nullable: false,
        })
    }

    pub fn null(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Null,
            nullable: true,
        })
    }

    pub fn any(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Any,
            nullable: true,
        })
    }

    pub fn never(&mut self) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Never,
            nullable: false,
        })
    }

    pub fn named(&mut self, name: impl Into<String>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Named { name: name.into() },
            nullable: false,
        })
    }

    pub fn generic(&mut self, name: impl Into<String>, args: Vec<TypeId>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Generic {
                name: name.into(),
                args,
            },
            nullable: false,
        })
    }

    pub fn generic_param(&mut self, name: impl Into<String>, owner: GenericOwner) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::GenericParam {
                name: name.into(),
                owner,
            },
            nullable: false,
        })
    }

    pub fn lambda(&mut self, params: Vec<TypeId>, ret: TypeId) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Lambda(LambdaType { params, ret }),
            nullable: false,
        })
    }

    pub fn tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        self.alloc(Type {
            kind: TypeKind::Tuple { elements },
            nullable: false,
        })
    }
}

// =============================================================================
// Type registry — holds module-level declared types
// =============================================================================

/// Looks up named types. P2.5/P2.6 will populate this from HIR + stdlib.
#[derive(Debug, Default)]
pub struct TypeRegistry {
    /// Maps simple type name -> a Named TypeId in the arena.
    named: HashMap<String, TypeId>,
}

impl TypeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: impl Into<String>, id: TypeId) {
        self.named.insert(name.into(), id);
    }

    pub fn lookup(&self, name: &str) -> Option<TypeId> {
        self.named.get(name).copied()
    }
}

// =============================================================================
// Subtyping
// =============================================================================

/// `true` iff a value of `from` is assignable to a slot expecting `to`.
/// The relation handles primitive widening (int → float), nullability
/// (T → T?), top/bottom (anything → any, never → anything), and shape
/// matches for generics / tuples / lambdas. User-declared generics are
/// invariant in their parameters (TS reference behavior).
///
/// Returns `false` for shapes the relation hasn't been formally taught
/// — better to under-accept and surface false negatives in P2.5 than to
/// silently widen.
pub fn is_assignable_to(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    if from == to {
        return true;
    }
    let a = arena.get(from);
    let b = arena.get(to);

    // Null handling: `null` flows into anything nullable.
    if matches!(a.kind, TypeKind::Null) {
        return b.nullable;
    }
    // `never` flows everywhere.
    if matches!(a.kind, TypeKind::Never) {
        return true;
    }
    // `any` is the top type — everything flows into it.
    if matches!(b.kind, TypeKind::Any) {
        return true;
    }
    // A non-nullable can't widen into a different non-nullable type just
    // because of nullability difference: `T → T?` is fine, `T? → T` is not.
    if a.nullable && !b.nullable {
        return false;
    }

    // P7.3 node tagging: `node<T>` / `nodeTime<T>` / etc. auto-deref to
    // their inner `T`. The reverse direction stays asymmetric — a bare
    // `T` cannot promote to a tagged-node form without an explicit
    // constructor call.
    if let TypeKind::Generic { name, args } = &a.kind
        && is_node_tag(name)
        && args.len() == 1
        && is_assignable_to(arena, args[0], to)
    {
        return true;
    }

    match (&a.kind, &b.kind) {
        (TypeKind::Primitive(pa), TypeKind::Primitive(pb)) => primitive_assignable(*pa, *pb),
        (TypeKind::Named { name: na }, TypeKind::Named { name: nb }) => na == nb,
        (TypeKind::Generic { name: na, args: aa }, TypeKind::Generic { name: nb, args: ab }) => {
            // P12.2: invariant in every generic parameter. The TS
            // reference checker (`GreycatGenericType.isAssignableTo`)
            // implements covariance, but the GreyCat runtime — the
            // true oracle — rejects covariant assignment (e.g.
            // `Array<float>` is *not* assignable to `Array<int>`).
            // We follow the runtime, not the TS checker. Supertype-
            // chain assignability across different generic names
            // (`type Child<T> extends Parent<T>`) is a later phase.
            na == nb && aa.len() == ab.len() && aa.iter().zip(ab).all(|(x, y)| x == y)
        }
        // P7.5 anonymous structural compat: a value of `{a: A, b: B}`
        // is assignable to `{a: A}` (width subtyping — source may have
        // *extra* fields). Each shared field's source type must be
        // assignable to the target's field type.
        (TypeKind::Anonymous { fields: fa }, TypeKind::Anonymous { fields: fb }) => {
            fb.iter().all(|(name, want)| {
                fa.iter()
                    .find(|(n, _)| n == name)
                    .is_some_and(|(_, got)| is_assignable_to(arena, *got, *want))
            })
        }
        (TypeKind::Tuple { elements: ea }, TypeKind::Tuple { elements: eb }) => {
            ea.len() == eb.len()
                && ea
                    .iter()
                    .zip(eb)
                    .all(|(x, y)| is_assignable_to(arena, *x, *y))
        }
        (TypeKind::Lambda(la), TypeKind::Lambda(lb)) => {
            // Contravariant in params, covariant in return. Same as TS.
            la.params.len() == lb.params.len()
                && la
                    .params
                    .iter()
                    .zip(&lb.params)
                    .all(|(p_a, p_b)| is_assignable_to(arena, *p_b, *p_a))
                && is_assignable_to(arena, la.ret, lb.ret)
        }
        (TypeKind::Union { alts }, _) => {
            // Union assigns into `to` iff every alt does.
            alts.iter().all(|a| is_assignable_to(arena, *a, to))
        }
        (_, TypeKind::Union { alts }) => {
            // Single value flows into a union if it matches *any* alt.
            alts.iter().any(|b| is_assignable_to(arena, from, *b))
        }
        (TypeKind::Enum { name: na, .. }, TypeKind::Enum { name: nb, .. }) => na == nb,
        _ => false,
    }
}

/// Primitive widening lattice: `int -> float`, plus identity. Strings,
/// chars, bools etc. don't widen.
/// `true` for any of the runtime "node-tag" generic names that
/// auto-deref to their inner type in the assignability relation
/// (P7.3). Drawn from the TS reference's `StdCoreTypes` interface.
pub fn is_node_tag(name: &str) -> bool {
    matches!(
        name,
        "node" | "nodeTime" | "nodeGeo" | "nodeList" | "nodeIndex"
    )
}

// =============================================================================
// Inference table (P7.4 — foundational pass)
// =============================================================================

/// Per-call constraint table that records "type-parameter `T` was
/// witnessed at type `…`" pairs as the analyzer walks a generic call
/// site. After all arguments have been visited, [`InferenceTable::solve`]
/// substitutes accumulated witnesses into the declared return type.
///
/// **Scope:** records and substitutes simple `GenericParam` ↔ concrete
/// pairs. Constraint propagation (e.g. `T : SomeBound` requiring the
/// witness to satisfy the bound), variance handling beyond what
/// [`is_assignable_to`] already provides, and union-of-witnesses
/// merging are deferred — this is the seam, not a full Hindley-Milner.
#[derive(Debug, Default)]
pub struct InferenceTable {
    bindings: HashMap<String, TypeId>,
}

impl InferenceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a witness for a generic param. If the same param has
    /// already been bound, the new witness is dropped — the analyzer's
    /// caller should already have type-checked it against the prior
    /// witness through [`is_assignable_to`].
    pub fn bind(&mut self, name: impl Into<String>, ty: TypeId) {
        self.bindings.entry(name.into()).or_insert(ty);
    }

    pub fn lookup(&self, name: &str) -> Option<TypeId> {
        self.bindings.get(name).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Substitute every `GenericParam(name)` in `ty` with the recorded
    /// witness. Idempotent — re-applying produces the same result.
    pub fn substitute(&self, arena: &mut TypeArena, ty: TypeId) -> TypeId {
        let t = arena.get(ty).clone();
        match &t.kind {
            TypeKind::GenericParam { name, .. } => {
                if let Some(witness) = self.bindings.get(name) {
                    let nullable = t.nullable;
                    if !nullable {
                        return *witness;
                    }
                    arena.nullable(*witness)
                } else {
                    ty
                }
            }
            TypeKind::Generic { name, args } => {
                let new_args: Vec<TypeId> =
                    args.iter().map(|a| self.substitute(arena, *a)).collect();
                if new_args == *args {
                    ty
                } else {
                    let name = name.clone();
                    let mut new_t = arena.generic(name, new_args);
                    if t.nullable {
                        new_t = arena.nullable(new_t);
                    }
                    new_t
                }
            }
            TypeKind::Tuple { elements } => {
                let new_els: Vec<TypeId> = elements
                    .iter()
                    .map(|e| self.substitute(arena, *e))
                    .collect();
                if new_els == *elements {
                    ty
                } else {
                    let mut new_t = arena.tuple(new_els);
                    if t.nullable {
                        new_t = arena.nullable(new_t);
                    }
                    new_t
                }
            }
            _ => ty,
        }
    }
}

/// `true` iff `from` can be casted to `to` via the GreyCat `as` operator.
///
/// Mirrors the TS reference's `isCastable` (`packages/lang/src/analysis/
/// utils.ts:360`). Cast rules are asymmetric to assignability — `int as
/// nodeTime` is allowed even though `int` doesn't assign-flow into
/// `nodeTime`. Implements (P12.3 — deeper node-tag rules):
/// - `any → any` always.
/// - Nullables: `T?` casts the same as `T`.
/// - `int ↔ {int, float, node, nodeTime, nodeList, nodeIndex, nodeGeo}`.
/// - `float ↔ {int, float}`.
/// - `node{,Time,List,Index,Geo} ↔ {self, int}`.
/// - `String ↔ String`.
/// - `char ↔ {char, String, int}`.
/// - `bool ↔ bool`.
/// - `t{2,3,4}{,f} → int`.
/// - Enums → `int`.
/// - Anything else falls through to "same head name OR `from` assignable
///   to `to` (no inheritance check yet — that lands when supertype
///   chains thread through the analyzer)".
pub fn is_castable(arena: &TypeArena, from: TypeId, to: TypeId) -> bool {
    let from_t = arena.get(from);
    let to_t = arena.get(to);

    // any target absorbs any non-null source.
    if matches!(to_t.kind, TypeKind::Any) && !from_t.nullable {
        return true;
    }

    // Union: cast iff any alt casts. Source nullability is otherwise
    // ignored — the TS reference's `from = from.nn()` strip is purely
    // about treating `T?` like `T` for kind dispatch, which we get
    // for free by reading `from_t.kind` directly.
    if let TypeKind::Union { alts } = &from_t.kind {
        return alts.iter().any(|a| is_castable(arena, *a, to));
    }
    if matches!(from_t.kind, TypeKind::Enum { .. }) && is_int_target(to_t) {
        return true;
    }

    let to_head = generic_or_named_name(to_t);
    match &from_t.kind {
        TypeKind::Any => true,
        TypeKind::Primitive(Primitive::Int) => {
            matches!(
                to_head.as_deref(),
                Some("node" | "nodeTime" | "nodeList" | "nodeIndex" | "nodeGeo")
            ) || is_primitive(to_t, Primitive::Int)
                || is_primitive(to_t, Primitive::Float)
        }
        TypeKind::Primitive(Primitive::Float) => {
            is_primitive(to_t, Primitive::Int) || is_primitive(to_t, Primitive::Float)
        }
        TypeKind::Primitive(Primitive::String) => is_primitive(to_t, Primitive::String),
        TypeKind::Primitive(Primitive::Char) => {
            is_primitive(to_t, Primitive::Char)
                || is_primitive(to_t, Primitive::String)
                || is_primitive(to_t, Primitive::Int)
        }
        TypeKind::Primitive(Primitive::Bool) => is_primitive(to_t, Primitive::Bool),
        // node-tag heads: cast to int or to themselves (covariant
        // generic args via the `same head name` branch — narrows are
        // P12.4 territory).
        TypeKind::Generic { name, .. } | TypeKind::Named { name } if is_node_tag(name) => {
            is_int_target(to_t) || matches!(to_head.as_deref(), Some(n) if n == name)
        }
        // Tuple primitives → int.
        TypeKind::Generic { name, .. } | TypeKind::Named { name }
            if matches!(name.as_str(), "t2" | "t3" | "t4" | "t2f" | "t3f" | "t4f") =>
        {
            is_int_target(to_t)
        }
        _ => is_assignable_to(arena, from, to),
    }
}

fn generic_or_named_name(t: &Type) -> Option<String> {
    match &t.kind {
        TypeKind::Generic { name, .. } | TypeKind::Named { name } => Some(name.clone()),
        TypeKind::Primitive(p) => Some(p.name().to_string()),
        _ => None,
    }
}

fn is_primitive(t: &Type, p: Primitive) -> bool {
    matches!(t.kind, TypeKind::Primitive(q) if q == p)
}

fn is_int_target(t: &Type) -> bool {
    is_primitive(t, Primitive::Int)
}

fn primitive_assignable(from: Primitive, to: Primitive) -> bool {
    if from == to {
        return true;
    }
    matches!((from, to), (Primitive::Int, Primitive::Float))
}

// =============================================================================
// Display
// =============================================================================

pub fn display(arena: &TypeArena, id: TypeId) -> String {
    let ty = arena.get(id);
    let mut s = match &ty.kind {
        TypeKind::Null => "null".to_string(),
        TypeKind::Any => "any".to_string(),
        TypeKind::Never => "never".to_string(),
        TypeKind::Primitive(p) => p.name().to_string(),
        TypeKind::Named { name } => name.clone(),
        TypeKind::Generic { name, args } => {
            let parts: Vec<String> = args.iter().map(|a| display(arena, *a)).collect();
            format!("{name}<{}>", parts.join(", "))
        }
        TypeKind::GenericParam { name, .. } => name.clone(),
        TypeKind::Lambda(l) => {
            let parts: Vec<String> = l.params.iter().map(|p| display(arena, *p)).collect();
            format!("({}) -> {}", parts.join(", "), display(arena, l.ret))
        }
        TypeKind::Tuple { elements } => {
            let parts: Vec<String> = elements.iter().map(|e| display(arena, *e)).collect();
            format!("({})", parts.join(", "))
        }
        TypeKind::Anonymous { fields } => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(n, t)| format!("{n}: {}", display(arena, *t)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
        TypeKind::Enum { name, .. } => name.clone(),
        TypeKind::Union { alts } => {
            let parts: Vec<String> = alts.iter().map(|a| display(arena, *a)).collect();
            parts.join(" | ")
        }
    };
    if ty.nullable && !matches!(ty.kind, TypeKind::Null | TypeKind::Any) {
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> TypeArena {
        TypeArena::new()
    }

    #[test]
    fn intern_collapses_equal_types() {
        let mut a = fresh();
        let i1 = a.primitive(Primitive::Int);
        let i2 = a.primitive(Primitive::Int);
        assert_eq!(i1, i2);
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn nullable_idempotent() {
        let mut a = fresh();
        let i = a.primitive(Primitive::Int);
        let q1 = a.nullable(i);
        let q2 = a.nullable(q1);
        assert_eq!(q1, q2);
    }

    #[test]
    fn int_widens_to_float() {
        let mut a = fresh();
        let i = a.primitive(Primitive::Int);
        let f = a.primitive(Primitive::Float);
        assert!(is_assignable_to(&a, i, f));
        assert!(!is_assignable_to(&a, f, i));
    }

    #[test]
    fn null_flows_into_nullable_only() {
        let mut a = fresh();
        let null = a.null();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        assert!(is_assignable_to(&a, null, int_q));
        assert!(!is_assignable_to(&a, null, int));
    }

    #[test]
    fn nullable_does_not_silently_narrow() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        assert!(is_assignable_to(&a, int, int_q));
        assert!(!is_assignable_to(&a, int_q, int));
    }

    #[test]
    fn any_top_never_bottom() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let any = a.any();
        let never = a.never();
        assert!(is_assignable_to(&a, int, any));
        assert!(is_assignable_to(&a, never, int));
    }

    #[test]
    fn generic_invariant_in_args() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let float = a.primitive(Primitive::Float);
        let arr_int = a.generic("Array", vec![int]);
        let arr_float = a.generic("Array", vec![float]);
        // P12.2 (matches the GreyCat runtime, *not* the TS reference
        // checker): generic args are invariant. Even though `int`
        // widens to `float`, `Array<int>` is **not** assignable to
        // `Array<float>` (the runtime rejects this — we trust the
        // runtime as the oracle). The reverse is also rejected.
        assert!(!is_assignable_to(&a, arr_int, arr_float));
        assert!(!is_assignable_to(&a, arr_float, arr_int));
        assert!(is_assignable_to(&a, arr_int, arr_int));
    }

    #[test]
    fn generic_name_mismatch_stays_unassignable() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let arr_int = a.generic("Array", vec![int]);
        let set_int = a.generic("Set", vec![int]);
        // Different generic names with the same args still mismatch.
        // Inheritance-aware assignability (`type Child<T> extends
        // Parent<T>`) is a later phase.
        assert!(!is_assignable_to(&a, arr_int, set_int));
    }

    #[test]
    fn lambda_contravariant_params_covariant_return() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let float = a.primitive(Primitive::Float);
        // (float) -> int  is assignable to  (int) -> float
        // Param: int (target) -> float (source) yes (int widens to float).
        // Return: int (source) -> float (target) yes (int widens to float).
        let f1 = a.lambda(vec![float], int);
        let f2 = a.lambda(vec![int], float);
        assert!(is_assignable_to(&a, f1, f2));
        assert!(!is_assignable_to(&a, f2, f1));
    }

    #[test]
    fn union_member_flows_in() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let str_t = a.primitive(Primitive::String);
        let union = a.alloc(Type {
            kind: TypeKind::Union {
                alts: vec![int, str_t],
            },
            nullable: false,
        });
        assert!(is_assignable_to(&a, int, union));
        assert!(is_assignable_to(&a, str_t, union));
        let bool_t = a.primitive(Primitive::Bool);
        assert!(!is_assignable_to(&a, bool_t, union));
    }

    #[test]
    fn registry_lookup() {
        let mut a = fresh();
        let mut reg = TypeRegistry::new();
        let foo = a.named("Foo");
        reg.register("Foo", foo);
        assert_eq!(reg.lookup("Foo"), Some(foo));
        assert!(reg.lookup("Bar").is_none());
    }

    #[test]
    fn display_renders_nullable_suffix() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let int_q = a.nullable(int);
        let str_t = a.primitive(Primitive::String);
        let arr = a.generic("Array", vec![str_t]);
        assert_eq!(display(&a, int_q), "int?");
        assert_eq!(display(&a, arr), "Array<String>");
    }

    #[test]
    fn node_tag_auto_derefs_to_inner() {
        let mut a = fresh();
        let person = a.named("Person");
        let node_person = a.generic("node", vec![person]);
        // node<Person> → Person  (auto-deref)
        assert!(is_assignable_to(&a, node_person, person));
        // Person → node<Person>  is NOT auto-promoted.
        assert!(!is_assignable_to(&a, person, node_person));
    }

    #[test]
    fn inference_table_substitutes_generic_params() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let t_param = a.alloc(Type {
            kind: TypeKind::GenericParam {
                name: "T".into(),
                owner: GenericOwner::Type("Foo".into()),
            },
            nullable: false,
        });
        let arr_t = a.generic("Array", vec![t_param]);

        let mut tbl = InferenceTable::new();
        tbl.bind("T", int);

        let resolved = tbl.substitute(&mut a, arr_t);
        let resolved_kind = &a.get(resolved).kind;
        let TypeKind::Generic { name, args } = resolved_kind else {
            panic!("expected Array<int>, got {resolved_kind:?}");
        };
        assert_eq!(name, "Array");
        assert_eq!(args, &vec![int]);
    }

    #[test]
    fn anonymous_width_subtyping() {
        let mut a = fresh();
        let int = a.primitive(Primitive::Int);
        let str_t = a.primitive(Primitive::String);
        let two = a.alloc(Type {
            kind: TypeKind::Anonymous {
                fields: vec![("a".into(), int), ("b".into(), str_t)],
            },
            nullable: false,
        });
        let one = a.alloc(Type {
            kind: TypeKind::Anonymous {
                fields: vec![("a".into(), int)],
            },
            nullable: false,
        });
        // {a, b} → {a}  (width subtyping: extra field b is fine)
        assert!(is_assignable_to(&a, two, one));
        // {a} → {a, b}  is NOT — would be missing field b.
        assert!(!is_assignable_to(&a, one, two));
    }
}
