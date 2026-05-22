//! HIR node types — declarations, statements, expressions, type refs.
//! "Type ref" here means *syntactic* type annotation (e.g. `Array<int>`),
//! distinct from the *semantic* `Type` enum that `greycat-analyzer-core`
//! computes during inference.

use std::ops::Range;

use greycat_analyzer_core::Symbol;

use crate::arena::Idx;

pub type Span = Range<usize>;

/// The whole HIR for a single source file. All `Idx<…>` handles in this
/// module index into one of the arenas held by [`crate::Hir`].
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub lib: String,
    pub decls: Box<[Idx<Decl>]>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct Ident {
    pub symbol: Symbol,
    pub byte_range: Span,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Modifiers {
    pub private: bool,
    pub static_: bool,
    pub abstract_: bool,
    pub native: bool,
    // P13.4
    /// Annotations declared on this decl, drawn from grammar
    /// `annotations`. Each entry carries the annotation name plus
    /// any primitive-literal arguments — `@expose("renamed")`,
    /// `@tag("mcp")`, `@max_count(100)`, `@enabled(true)`,
    /// `@timeout(5s)`, etc. See [`AnnotationArg`] for the per-arg
    /// shape.
    pub annotations: Box<[Annotation]>,
}

/// Decl annotation — `@<name>(<args>...)`.
///
/// Both the annotation name and any string-literal args are
/// interned through the project's [`SymbolTable`](crate::Symbol)
/// (no `SmolStr`/`String`). Annotation strings are literal — there
/// is no interpolation, no `${...}` — and they repeat heavily
/// across decls (`@expose`, `@tag("mcp")`, `@permission("admin")`
/// on dozens of fns), so interning them is a straight win.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Annotation name as an interned symbol (e.g. `expose`,
    /// `tag`, `permission`).
    pub name: Symbol,
    pub args: Box<[AnnotationArg]>,
}

impl Annotation {
    /// Iterator over the string-typed arguments only, in source
    /// order. Convenience for callers (`@expose`, `@deref`,
    /// `@iterable`) that only care about string args.
    pub fn arg_strings(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.args.iter().filter_map(|a| match a {
            AnnotationArg::String(s) => Some(*s),
            _ => None,
        })
    }

    /// First string-typed arg, if any.
    pub fn first_string_arg(&self) -> Option<Symbol> {
        self.arg_strings().next()
    }
}

/// Compile-time-constant argument of an [`Annotation`]. GreyCat
/// pragmas accept only values the analyzer can resolve without
/// running code: primitive literals, `null`, and path-shaped
/// references to types or enum variants (`Foo`, `mod::Foo`,
/// `DurationUnit::milliseconds`).
///
/// Anything else — a call, arithmetic, an array literal, an
/// instance member-access, etc. — is captured as
/// [`AnnotationArg::Invalid`] so the analyzer can surface it as a
/// hard `invalid-pragma-arg` error pointing at the offending span.
/// Non-resolving paths get the same treatment at validation time.
///
/// `Float` is bit-equal for `Hash` so identical NaN payloads dedup
/// (matches the literal interning we already do for `LiteralExpr`).
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotationArg {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    /// String args are interned through the project's
    /// `SymbolTable` — `@tag("mcp")` repeated 50 times shares one
    /// `Symbol`.
    String(Symbol),
    /// Microseconds (GreyCat's canonical `duration` unit).
    Duration(i64),
    /// Microseconds since the Unix epoch.
    Time(i64),
    /// Microseconds since the Unix epoch — variant preserved so
    /// the consumer can distinguish
    /// `@since("2024-01-01T00:00:00Z")` from a raw numeric `time`.
    Iso8601(i64),
    /// The `null` literal.
    Null,
    /// Path expression — `Foo`, `mod::Foo`, `Foo::bar`,
    /// `mod::Foo::bar`. The `chain` segments are the parsed
    /// identifiers in source order; the analyzer's validator
    /// resolves the path to either a type decl or an enum variant
    /// at validation time. Unresolved paths surface as a hard
    /// `invalid-pragma-arg` error pointing at `start..end`.
    Path {
        chain: Box<[Symbol]>,
        start: u32,
        end: u32,
    },
    /// Structurally-non-constant argument — a call, arithmetic, an
    /// array / object literal, an instance member-access, etc.
    /// Hard error at validation time pointing at `start..end`.
    Invalid {
        start: u32,
        end: u32,
    },
}

impl Eq for AnnotationArg {}

impl std::hash::Hash for AnnotationArg {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        // Discriminant + payload bits. Float hashes via bit pattern
        // so two `Float(f)` with the same bits dedup; NaN payloads
        // compare unequal under `PartialEq` but the hasher still
        // distributes them consistently — fine for dedup tables.
        std::mem::discriminant(self).hash(h);
        match self {
            AnnotationArg::Int(v)
            | AnnotationArg::Duration(v)
            | AnnotationArg::Time(v)
            | AnnotationArg::Iso8601(v) => v.hash(h),
            AnnotationArg::Float(f) => f.to_bits().hash(h),
            AnnotationArg::Bool(b) => b.hash(h),
            AnnotationArg::Char(c) => c.hash(h),
            AnnotationArg::String(s) => s.hash(h),
            AnnotationArg::Null => {}
            AnnotationArg::Path { chain, .. } => {
                // start/end deliberately excluded from the hash —
                // two paths with the same chain but different source
                // spans dedup to the same value.
                chain.hash(h);
            }
            AnnotationArg::Invalid { start, end } => {
                start.hash(h);
                end.hash(h);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum Decl {
    Fn(FnDecl),
    Type(TypeDecl),
    Enum(EnumDecl),
    Var(VarDeclTop),
    Pragma(Pragma),
}

impl Decl {
    pub fn name(&self) -> Option<Idx<Ident>> {
        match self {
            Decl::Fn(d) => Some(d.name),
            Decl::Type(d) => Some(d.name),
            Decl::Enum(d) => Some(d.name),
            Decl::Var(d) => Some(d.name),
            Decl::Pragma(p) => Some(p.name),
        }
    }
    pub fn byte_range(&self) -> &Span {
        match self {
            Decl::Fn(d) => &d.byte_range,
            Decl::Type(d) => &d.byte_range,
            Decl::Enum(d) => &d.byte_range,
            Decl::Var(d) => &d.byte_range,
            Decl::Pragma(p) => &p.byte_range,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: Idx<Ident>,
    pub modifiers: Modifiers,
    /// Generic type parameters (`fn foo<T, U>(...)`). The grammar
    /// allows any arity; the analyzer rejects >2 to match the runtime
    /// (`Map<K, V>` is the widest the runtime supports).
    pub generics: Box<[Idx<Ident>]>,
    pub params: Box<[Idx<FnParam>]>,
    pub return_type: Option<Idx<TypeRef>>,
    /// `None` for native functions.
    pub body: Option<Idx<Stmt>>, // a Block stmt
    pub doc: Option<String>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct FnParam {
    pub name: Idx<Ident>,
    pub ty: Option<Idx<TypeRef>>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: Idx<Ident>,
    pub modifiers: Modifiers,
    /// Generic type parameters (`type Foo<T, U> {}`). Same arity
    /// caveat as [`FnDecl::generics`] — grammar accepts any number,
    /// analyzer rejects >2.
    pub generics: Box<[Idx<Ident>]>,
    pub supertype: Option<Idx<TypeRef>>,
    pub attrs: Box<[Idx<TypeAttr>]>,
    /// Methods declared on the type. Each entry is a `Decl::Fn`
    /// (with `static_` / `abstract_` etc.).
    pub methods: Box<[Idx<Decl>]>,
    pub doc: Option<String>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct TypeAttr {
    pub name: Idx<Ident>,
    pub modifiers: Modifiers,
    pub ty: Option<Idx<TypeRef>>,
    pub init: Option<Idx<Expr>>,
    pub doc: Option<String>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: Idx<Ident>,
    pub modifiers: Modifiers,
    pub fields: Box<[Idx<EnumField>]>,
    pub doc: Option<String>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct EnumField {
    pub name: Idx<Ident>,
    pub value: Option<Idx<Expr>>,
    pub byte_range: Span,
}

/// Top-level `var`/`modvar` declaration.
#[derive(Debug, Clone)]
pub struct VarDeclTop {
    pub name: Idx<Ident>,
    pub modifiers: Modifiers,
    pub ty: Option<Idx<TypeRef>>,
    pub init: Option<Idx<Expr>>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct Pragma {
    pub name: Idx<Ident>,
    pub args: Box<[Idx<Expr>]>,
    pub byte_range: Span,
}

// =============================================================================
// Statements
// =============================================================================

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Idx<Expr>),
    Block(BlockStmt),
    Var(LocalVar),
    Assign(AssignStmt),
    If(IfStmt),
    While(WhileStmt),
    DoWhile(DoWhileStmt),
    For(ForStmt),
    ForIn(ForInStmt),
    Return(Option<Idx<Expr>>),
    Break,
    Continue,
    Breakpoint,
    Throw(Idx<Expr>),
    Try(TryStmt),
    At(AtStmt),
}

/// `{ … }` block. Carries its own `byte_range` (the curly-brace
/// span as parsed) so capabilities can bracket cursor-in-body
/// without falling back to "first stmt..last stmt" — the latter
/// returns `0..0` for an empty body, which silently broke
/// scope-aware completion inside `for { }` / `try { }` / empty
/// branches.
#[derive(Debug, Clone)]
pub struct BlockStmt {
    pub stmts: Box<[Idx<Stmt>]>,
    pub byte_range: Span,
}

/// Local `var name: T = init;` inside a function body.
#[derive(Debug, Clone)]
pub struct LocalVar {
    pub name: Idx<Ident>,
    pub ty: Option<Idx<TypeRef>>,
    pub init: Option<Idx<Expr>>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct AssignStmt {
    pub target: Idx<Expr>,
    pub op: AssignOp,
    pub value: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Eq,
    AddEq,
    SubEq,
    MulEq,
    DivEq,
    ModEq,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub condition: Idx<Expr>,
    /// `if (cond) { … }` body. Always a block per grammar — held
    /// inline (rather than as `Idx<Stmt>` pointing at a `Stmt::Block`)
    /// so callers reach the curly-brace span without an extra arena
    /// lookup, and so the type system enforces the always-a-block
    /// invariant.
    pub then_branch: BlockStmt,
    /// `else` branch. Either an `else { … }` block or a nested `if`
    /// for else-if chains, hence the `Idx<Stmt>` polymorphism — the
    /// only field that's *not* always a block.
    pub else_branch: Option<Idx<Stmt>>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub condition: Idx<Expr>,
    pub body: BlockStmt,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct DoWhileStmt {
    pub body: BlockStmt,
    pub condition: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct ForStmt {
    pub init_name: Option<Idx<Ident>>,
    pub init_ty: Option<Idx<TypeRef>>,
    pub init_value: Option<Idx<Expr>>,
    pub condition: Option<Idx<Expr>>,
    pub increment: Option<Idx<Expr>>,
    pub body: BlockStmt,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct ForInStmt {
    /// Binders introduced by this for-in. The grammar's `sepBy2`
    /// guarantees `params.len() >= 2` — typically `(index, value)` or
    /// `(key, value)`.
    pub params: Box<[ForInParam]>,
    pub range: Idx<Expr>,
    pub body: BlockStmt,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct ForInParam {
    pub name: Idx<Ident>,
    pub ty: Option<Idx<TypeRef>>,
}

#[derive(Debug, Clone)]
pub struct TryStmt {
    pub try_block: BlockStmt,
    pub error_param: Option<Idx<Ident>>,
    pub catch_block: BlockStmt,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct AtStmt {
    pub expr: Idx<Expr>,
    pub block: BlockStmt,
    pub byte_range: Span,
}

// =============================================================================
// Expressions
// =============================================================================

#[derive(Debug, Clone)]
pub enum Expr {
    /// A bare-ident expression (a name used in expression position).
    ///
    /// Carries `byte_range` inline so [`Expr::byte_range`] is honest for
    /// every variant. The same span lives on the underlying
    /// `Ident` arena entry (which also serves declaration-site names,
    /// fn-param names, property names, type-ref names — anywhere an
    /// `Idx<Ident>` appears without an enclosing `Expr::Ident`); the
    /// two are written from the same `tree_sitter::Node` at lowering
    /// time and the Ident arena is grow-only, so they can't drift.
    Ident {
        name: Idx<Ident>,
        byte_range: Span,
    },
    /// Literal value — numeric, char, bool, duration, time, iso8601.
    /// Each carries its parsed value directly (see [`LiteralKind`]);
    /// the source text is no longer kept in the HIR.
    Literal(LiteralExpr),
    /// `null` keyword literal.
    Null {
        byte_range: Span,
    },
    /// `this` keyword reference. Types as the enclosing
    /// `TypeDecl`'s self type during analysis.
    This {
        byte_range: Span,
    },
    String(StringExpr),
    Tuple(Box<[Idx<Expr>]>, Span),
    Array(Box<[Idx<Expr>]>, Span),
    Object(ObjectExpr),
    Member(MemberExpr),
    Arrow(MemberExpr), // `n->name` — same shape, different access semantics
    Static(StaticExpr),
    // P15.8
    /// Chained `module::Type::method` (or longer). The
    /// HIR `StaticExpr` only models `Type::name` because its `ty`
    /// slot is a `TypeRef` and the grammar allows a nested
    /// `static_expr` as the head. For chains the lowering emits
    /// this flat-`Vec<Idx<Ident>>` shape instead. Each segment is
    /// an `ident` from the source. Length is always >= 2.
    QualifiedStatic {
        chain: Box<[Idx<Ident>]>,
        byte_range: Span,
    },
    Offset(OffsetExpr),
    Call(CallExpr),
    Binary(BinaryExpr),
    Unary(UnaryExpr),
    Paren(Idx<Expr>, Span),
    Lambda(LambdaExpr),
    // P19.15
    /// `from..to` (or `from..` / `..to`) range and the
    /// math-style `]from..to]` / `[from..to[` interval. Both forms
    /// flatten into the same HIR node since the bracket markers
    /// don't change typing — they only matter at runtime for
    /// inclusivity. Used as the index of an `Expr::Offset` to
    /// signal a slice (`arr[1..10]` returns the same shape as `arr`)
    /// or as a for-in iterator-range clause.
    Range {
        from: Option<Idx<Expr>>,
        to: Option<Idx<Expr>>,
        byte_range: Span,
    },
    // P6.5
    /// `value is Type` — runtime type guard, evaluates to `bool`.
    /// Recognized by the analyzer to narrow `value` in the matching
    /// branch when used inside an `if` condition.
    Is {
        value: Idx<Expr>,
        ty: Idx<TypeRef>,
        byte_range: Span,
    },
    /// `value as Type` — type ascription / cast, evaluates to `Type`.
    /// The runtime semantics are a checked downcast; the analyzer just
    /// adopts the cast's declared type as the expression's type.
    Cast {
        value: Idx<Expr>,
        ty: Idx<TypeRef>,
        byte_range: Span,
    },
    /// Anything we haven't lowered yet — keeps the byte range so downstream
    /// passes can still gracefully skip. Will shrink as downstream stages
    /// demand more precise variants.
    Unsupported {
        kind: &'static str,
        byte_range: Span,
    },
}

impl Expr {
    pub fn byte_range(&self) -> Span {
        match self {
            Expr::Ident { byte_range, .. } => byte_range.clone(),
            Expr::Literal(l) => l.byte_range.clone(),
            Expr::Null { byte_range } => byte_range.clone(),
            Expr::This { byte_range } => byte_range.clone(),
            Expr::String(s) => s.byte_range.clone(),
            Expr::Tuple(_, r) | Expr::Array(_, r) | Expr::Paren(_, r) => r.clone(),
            Expr::Object(o) => o.byte_range.clone(),
            Expr::Member(m) | Expr::Arrow(m) => m.byte_range.clone(),
            Expr::Static(s) => s.byte_range.clone(),
            Expr::QualifiedStatic { byte_range, .. } => byte_range.clone(),
            Expr::Offset(o) => o.byte_range.clone(),
            Expr::Call(c) => c.byte_range.clone(),
            Expr::Binary(b) => b.byte_range.clone(),
            Expr::Unary(u) => u.byte_range.clone(),
            Expr::Lambda(l) => l.byte_range.clone(),
            Expr::Is { byte_range, .. } | Expr::Cast { byte_range, .. } => byte_range.clone(),
            Expr::Range { byte_range, .. } => byte_range.clone(),
            Expr::Unsupported { byte_range, .. } => byte_range.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiteralExpr {
    pub kind: LiteralKind,
    /// Source-side parse anomaly (overflow, precision loss, …). The
    /// `kind` field still carries a best-effort value so downstream
    /// typing proceeds normally; the analyzer reads this field
    /// separately to emit user-facing warnings / errors.
    pub parse_issue: Option<ParseIssue>,
    pub byte_range: Span,
}

/// Typed literal value. Each variant carries the parsed value
/// directly — lowering does the parsing once; downstream stages
/// dispatch on the variant tag without touching source text.
///
/// `null` and `this` are *not* literals — they're keyword tokens
/// with no value to inline. They live as dedicated [`Expr::Null`] /
/// [`Expr::This`] variants instead.
///
/// When the source text fails to parse (overflow, precision loss,
/// malformed escape, …) the variant still commits to a kind and the
/// best-effort value (`i64::MAX` saturation, `'\0'`, `0` µs, …); the
/// failure is reported via [`LiteralExpr::parse_issue`]. This keeps
/// type inference unaffected by parse anomalies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiteralKind {
    Int(i64),
    Float(f64),
    Char(char),
    Bool(bool),
    /// Duration in microseconds — GreyCat's canonical unit for
    /// `duration`. Sub-microsecond suffixes (`ns`, `nanosecond`)
    /// are truncated toward zero by the lowering.
    Duration(i64),
    /// Time in microseconds since the Unix epoch — matches the
    /// GreyCat runtime's storage for `time`.
    Time(i64),
    /// ISO-8601 time literal, eager-parsed to µs-since-epoch.
    /// Variant preserved (rather than folded into [`Self::Time`])
    /// so the analyzer can run ISO-specific diagnostics (deprecated
    /// forms, suspicious timezone offsets, etc.) that wouldn't make
    /// sense for a raw numeric `time` literal.
    Iso8601(i64),
}

/// Parse anomaly recorded at HIR-lowering time. The literal's
/// [`LiteralKind`] carries a best-effort value alongside; the
/// analyzer emits the user-facing diagnostic from this tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseIssue {
    /// Numeric exceeded the representable range of its kind
    /// (`Int` → i64, `Duration` / `Time` / `Iso8601` → i64 µs after
    /// suffix scaling). The kind's value is saturated.
    Overflow,
    /// Float has more significant decimal digits than f64 can
    /// represent exactly. The kind's value is the nearest f64.
    PrecisionLoss,
    /// Char escape unrecognised, ISO-8601 shape malformed, or
    /// similar structural defect. The kind's value is a placeholder
    /// (`'\0'`, `0` µs, …).
    Malformed,
}

#[derive(Debug, Clone)]
pub struct StringExpr {
    // P17.5
    /// `parts` carries the lowered text fragments and
    /// `${expr}` interpolation expressions in source order. A
    /// non-template string (no `${…}`) lowers to a single
    /// `StringPart::Lit` covering the inner text. Template strings
    /// lower to alternating `Lit` / `Interp` entries. Each part keeps
    /// its own byte range so the parity oracle / capabilities can
    /// emit per-fragment records (`RawStringExpr` /
    /// `InterpolationExpr`) and the resolver / analyzer can recurse
    /// into each `Interp.expr`.
    pub parts: Box<[StringPart]>,
    pub byte_range: Span,
}

impl StringExpr {
    /// Concatenated raw fragments — interpolation parts are skipped.
    /// Sufficient for "is this a string?" plus the few sites that need
    /// the literal value (e.g. `@permission("admin")` extraction in
    /// [`crate`]'s `stdlib::ProjectIndex::ingest`).
    pub fn raw_value(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let StringPart::Lit { text, .. } = p {
                out.push_str(text);
            }
        }
        out
    }

    /// `true` iff at least one part is a `${expr}` interpolation.
    pub fn has_interpolation(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, StringPart::Interp { .. }))
    }
}

// P17.5
/// One piece of a [`StringExpr`].
#[derive(Debug, Clone)]
pub enum StringPart {
    /// Raw text between (or around) interpolations. The byte range
    /// covers just the fragment in source — not the surrounding `"`
    /// quote chars or `${...}` markers.
    Lit { text: String, byte_range: Span },
    /// A `${expr}` interpolation. The inner expression lives in the
    /// HIR `exprs` arena. The byte range covers the whole `${expr}`
    /// (matches TS reference's `InterpolationExpr` span).
    Interp { expr: Idx<Expr>, byte_range: Span },
}

#[derive(Debug, Clone)]
pub struct ObjectExpr {
    pub ty: Option<Idx<TypeRef>>,
    pub fields: Box<[ObjectField]>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct ObjectField {
    pub name: Option<Idx<Ident>>,
    pub value: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct MemberExpr {
    pub receiver: Idx<Expr>,
    pub property: PropertyName,
    /// `a?.b` / `a?->b` — null-safe access. When `a: T?`, the result
    /// lifts to `(typeof a.b)?`; when `a: T`, the marker is a no-op.
    pub pre_optional: bool,
    /// `a.b?` / `a->b?` — explicit "treat as nullable" suffix on the
    /// access result. Lifts the result to nullable regardless of the
    /// declared field type.
    pub post_optional: bool,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct StaticExpr {
    pub ty: Idx<TypeRef>,
    pub property: PropertyName,
    pub byte_range: Span,
}

/// Property name in a `member_expr` / `arrow_expr` / `static_expr`.
///
/// Both forms resolve to the same field/method by name, so most
/// consumers should reach for [`PropertyName::ident`] and treat
/// either variant uniformly. Match on the variant only when the
/// syntactic form matters (e.g. diagnostics that quote the source,
/// or formatter round-trips).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyName {
    /// `a.b`, `a->b`, `T::b` — bareword identifier property.
    Ident(Idx<Ident>),
    /// `a."b.c"`, `a->"b.c"`, `T::"b.c"` — string-literal property.
    /// The pointed-to `Ident.symbol` resolves to the *decoded* property name
    /// (without surrounding quotes); `Ident.byte_range` covers the
    /// entire `"..."` literal in source.
    String(Idx<Ident>),
}

impl PropertyName {
    /// The interned ident carrying the property name's text + span.
    /// Both variants resolve to the same field/method, so most callers
    /// should use this and only match on the variant when they care
    /// about the syntactic form.
    #[inline]
    pub fn ident(self) -> Idx<Ident> {
        match self {
            PropertyName::Ident(i) | PropertyName::String(i) => i,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OffsetExpr {
    pub receiver: Idx<Expr>,
    pub index: Idx<Expr>,
    /// `a?[i]` — null-safe index access. When `a: T?`, the result
    /// lifts to nullable; when `a: T`, the marker is a no-op.
    pub pre_optional: bool,
    /// `a[i]?` — explicit "treat as nullable" suffix on the indexer.
    /// Lifts the result to nullable regardless.
    pub post_optional: bool,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: Idx<Expr>,
    pub args: Box<[Idx<Expr>]>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct BinaryExpr {
    pub op: BinOp,
    pub left: Idx<Expr>,
    pub right: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Coalesce, // ??
    /// Operator we recognized but haven't categorized. Carry the verbatim
    /// text so downstream can still process or reject it.
    Other(&'static str),
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub op: UnaryOp,
    pub operand: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    /// `+x` — identity (no-op on numeric operand). Grammar accepts it as
    /// a unary prefix; typing-wise it returns the operand type unchanged.
    Pos,
    Not,
    BitNot,
    /// `++x` / `x++` — increment. Returns the operand type (int / float).
    Inc,
    /// `--x` / `x--` — decrement. Returns the operand type (int / float).
    Dec,
    // P6.4
    /// `!!x` — non-null assertion (narrowing).
    NonNullAssert,
    /// `*n` — node deref. Returns the inner `T` of a
    /// `node<T>` / `nodeTime<T>` / similar tag-shaped receiver.
    /// Equivalent to `n.resolve()` for typing purposes but keeps
    /// the receiver non-null (the dot form returns `T?`).
    Deref,
}

#[derive(Debug, Clone)]
pub struct LambdaExpr {
    pub params: Box<[Idx<FnParam>]>,
    pub return_type: Option<Idx<TypeRef>>,
    pub body: BlockStmt,
    pub byte_range: Span,
}

// =============================================================================
// Type references (syntactic)
// =============================================================================

/// A syntactic type reference. Models the grammar's `type_ident` rule
/// faithfully: `(ident "::")* ident <generics>? "?"?`.
///
/// `qualifier` carries the module path prefix (zero or more segments,
/// leftmost-first). Empty slice = bare reference.
#[derive(Debug, Clone)]
pub struct TypeRef {
    /// Module-qualifier segments before the leaf name.
    /// `Foo` → `[]`; `b::Foo` → `[b]`; `a::b::Foo` → `[a, b]`.
    pub qualifier: Box<[Idx<Ident>]>,
    /// Leaf decl name (the final `field("name", $.ident)` segment).
    pub name: Idx<Ident>,
    /// Generic args (`Map<K, V>` → `[K, V]`). Empty for non-generic.
    pub params: Box<[Idx<TypeRef>]>,
    pub optional: bool,
    /// `true` when the source carried a leading `typeof` keyword,
    /// declaring the reference as a *type literal* (the runtime value
    /// IS a type, not an instance of one). The grammar admits the
    /// keyword in two positions — inside `type_ident` itself and on
    /// the param-slot side of `fn_param` — so the lowering checks
    /// both and collapses them onto this single flag.
    pub typeof_marker: bool,
    pub byte_range: Span,
}

impl TypeRef {
    /// `true` iff this ref carries a module path prefix.
    pub fn is_qualified(&self) -> bool {
        !self.qualifier.is_empty()
    }
}
