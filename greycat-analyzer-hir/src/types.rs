//! HIR node types — declarations, statements, expressions, type refs.
//! "Type ref" is a *syntactic* type annotation (e.g. `Array<int>`),
//! distinct from the *semantic* `Type` enum in `greycat-analyzer-core`.

use std::ops::Range;

use greycat_analyzer_core::Symbol;

use crate::arena::Idx;

pub type Span = Range<usize>;

/// Top-level module. All `Idx<…>` handles index into the arenas held
/// by [`crate::Hir`].
#[derive(Debug, Clone)]
pub struct Module {
    pub name: Symbol,
    pub lib: Symbol,
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
    /// Annotations on this decl (`@expose("renamed")`, `@tag("mcp")`,
    /// `@max_count(100)`, …). See [`AnnotationArg`] for the per-arg shape.
    pub annotations: Box<[Annotation]>,
}

/// Decl annotation — `@<name>(<args>...)`. Name and string-literal
/// args are interned through the project's
/// [`SymbolTable`](crate::Symbol). Annotation strings are literal —
/// no interpolation.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// Annotation name as an [`Ident`] — interned symbol plus the
    /// name token's span (lets the pragma validator point a diagnostic
    /// at the name itself).
    pub name: Ident,
    pub args: Box<[AnnotationArg]>,
}

impl Annotation {
    /// String-typed args only, in source order.
    pub fn arg_strings(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.args.iter().filter_map(|a| match a.kind {
            AnnotationArgKind::String(s) => Some(s),
            _ => None,
        })
    }

    /// First string-typed arg, if any.
    pub fn first_string_arg(&self) -> Option<Symbol> {
        self.arg_strings().next()
    }
}

// Equality ignores the name's source span — identity is name symbol
// + args.
impl PartialEq for Annotation {
    fn eq(&self, other: &Self) -> bool {
        self.name.symbol == other.name.symbol && self.args == other.args
    }
}

impl Eq for Annotation {}

/// A single annotation argument: its compile-time value
/// ([`AnnotationArgKind`]) plus its source [`Span`]. Equality and
/// hashing ignore `span` — identity is the value.
#[derive(Debug, Clone)]
pub struct AnnotationArg {
    pub kind: AnnotationArgKind,
    pub span: Span,
}

impl PartialEq for AnnotationArg {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Eq for AnnotationArg {}

impl std::hash::Hash for AnnotationArg {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.kind.hash(h);
    }
}

/// Compile-time-constant value of an [`AnnotationArg`]. Pragmas
/// accept only primitive literals, `null`, and path-shaped references
/// to types or enum variants (`Foo`, `mod::Foo`,
/// `DurationUnit::milliseconds`). Anything else becomes
/// [`AnnotationArgKind::Invalid`] (hard `invalid-pragma-arg` error).
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotationArgKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    /// Interned through the project's `SymbolTable`.
    String(Symbol),
    /// Microseconds (GreyCat's canonical `duration` unit).
    Duration(i64),
    /// Microseconds since the Unix epoch.
    Time(i64),
    /// Microseconds since the Unix epoch — variant preserved so the
    /// consumer can distinguish `@since("2024-01-01T00:00:00Z")` from
    /// a raw numeric `time`.
    Iso8601(i64),
    /// The `null` literal.
    Null,
    /// Path expression — `Foo`, `mod::Foo`, `Foo::bar`,
    /// `mod::Foo::bar`. `chain` segments are the parsed identifiers in
    /// source order; the validator resolves them to a type decl or
    /// enum variant. Unresolved → hard `invalid-pragma-arg` error.
    Path {
        chain: Box<[Symbol]>,
    },
    /// Structurally-non-constant argument (call, arithmetic, array /
    /// object literal, instance member-access, …). Hard error.
    Invalid,
}

impl Eq for AnnotationArgKind {}

impl std::hash::Hash for AnnotationArgKind {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        // Discriminant + payload bits; Float hashes via bit pattern.
        std::mem::discriminant(self).hash(h);
        match self {
            AnnotationArgKind::Int(v)
            | AnnotationArgKind::Duration(v)
            | AnnotationArgKind::Time(v)
            | AnnotationArgKind::Iso8601(v) => v.hash(h),
            AnnotationArgKind::Float(f) => f.to_bits().hash(h),
            AnnotationArgKind::Bool(b) => b.hash(h),
            AnnotationArgKind::Char(c) => c.hash(h),
            AnnotationArgKind::String(s) => s.hash(h),
            AnnotationArgKind::Null | AnnotationArgKind::Invalid => {}
            AnnotationArgKind::Path { chain } => chain.hash(h),
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
    /// Generic type parameters (`fn foo<T>(...)`). Grammar allows any
    /// arity; a fn accepts exactly one generic at runtime (the
    /// analyzer's `too-many-generics` check enforces it).
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
    /// Generic type parameters (`type Foo<T, U> {}`). Grammar accepts
    /// any number; analyzer rejects >2.
    pub generics: Box<[Idx<Ident>]>,
    pub supertype: Option<Idx<TypeRef>>,
    pub attrs: Box<[Idx<TypeAttr>]>,
    /// Methods declared on the type. Each entry is a `Decl::Fn`.
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
    Return(ReturnStmt),
    Break(BreakStmt),
    Continue(ContinueStmt),
    Breakpoint(BreakpointStmt),
    Throw(ThrowStmt),
    Try(TryStmt),
    At(AtStmt),
}

/// `return [expr];`. `byte_range` covers the whole stmt (keyword
/// through `;`), so lints can anchor on the keyword even for a bare
/// `return;`.
#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub value: Option<Idx<Expr>>,
    pub byte_range: Span,
}

/// `throw expr;`. `byte_range` covers the whole stmt.
#[derive(Debug, Clone)]
pub struct ThrowStmt {
    pub value: Idx<Expr>,
    pub byte_range: Span,
}

/// `break;` — keyword-only stmt.
#[derive(Debug, Clone)]
pub struct BreakStmt {
    pub byte_range: Span,
}

/// `continue;` — keyword-only stmt.
#[derive(Debug, Clone)]
pub struct ContinueStmt {
    pub byte_range: Span,
}

/// `breakpoint;` — keyword-only stmt.
#[derive(Debug, Clone)]
pub struct BreakpointStmt {
    pub byte_range: Span,
}

/// `{ … }` block. `byte_range` is the curly-brace span (covers an
/// empty body, unlike "first stmt..last stmt").
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
    /// `if (cond) { … }` body. Always a block per grammar, held inline.
    pub then_branch: BlockStmt,
    /// `else` branch — an `else { … }` block or a nested `if` for
    /// else-if chains, hence `Idx<Stmt>` rather than `BlockStmt`.
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
    /// Binders introduced by this for-in. `params.len() >= 2` (grammar
    /// `sepBy2`) — typically `(index, value)` or `(key, value)`.
    pub params: Box<[ForInParam]>,
    /// The iterable expression. Its type drives the binders' element
    /// types.
    pub iterator: Idx<Expr>,
    /// Optional `[from..to]` slice window (an [`Expr::Range`]); `None`
    /// for a full iteration.
    pub window: Option<Idx<Expr>>,
    /// The for-in `?` token (`for (.. in iter?)`): `Some` spans the `?`
    /// token, `None` means no token. Without it a nullable iterator
    /// fires `possibly-null`.
    pub nullable_iter: Option<Span>,
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

#[derive(Debug, Clone)]
pub enum Expr {
    /// A bare-ident expression. `byte_range` mirrors the underlying
    /// `Ident` arena entry's span.
    Ident {
        name: Idx<Ident>,
        byte_range: Span,
    },
    /// Literal value — numeric, char, bool, duration, time, iso8601.
    /// Each carries its parsed value (see [`LiteralKind`]).
    Literal(LiteralExpr),
    /// `null` keyword literal.
    Null {
        byte_range: Span,
    },
    /// `this` keyword reference. Types as the enclosing `TypeDecl`'s
    /// self type.
    This {
        byte_range: Span,
    },
    String(StringExpr),
    Tuple(Box<[Idx<Expr>]>, Span),
    Array(Box<[Idx<Expr>]>, Span),
    Object(ObjectExpr),
    PositionalObject(PositionalObjectExpr),
    Member(MemberExpr),
    Arrow(MemberExpr), // `n->name` — same shape, different access semantics
    Static(StaticExpr),
    /// Chained `module::Type::method` (or longer). [`StaticExpr`] only
    /// models `Type::name`; chains lower to this flat segment list
    /// instead. `chain.len() >= 2`.
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
    /// `from..to` (or `from..` / `..to`) range and the math-style
    /// `]from..to]` / `[from..to[` interval — both flatten here since
    /// bracket inclusivity doesn't affect typing. Used as an
    /// `Expr::Offset` index (slice) or a for-in iterator-range clause.
    Range {
        from: Option<Idx<Expr>>,
        to: Option<Idx<Expr>>,
        byte_range: Span,
    },
    /// `value is Type` — runtime type guard, evaluates to `bool`.
    /// Narrows `value` in the matching branch of an `if` condition.
    Is {
        value: Idx<Expr>,
        ty: Idx<TypeRef>,
        byte_range: Span,
    },
    /// `value as Type` — type ascription / cast, evaluates to `Type`.
    Cast {
        value: Idx<Expr>,
        ty: Idx<TypeRef>,
        byte_range: Span,
    },
    /// Not-yet-lowered shape. Keeps the byte range so downstream passes
    /// can skip it.
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
            Expr::PositionalObject(o) => o.byte_range.clone(),
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

    // pub fn is_pathy(&self) -> bool {
    //     matches!(self, Expr::Member(_) | Expr::Array(_, _) | Expr::Static(_) | Expr::Ident { .. } | Expr::Typ)
    // }
}

#[derive(Debug, Clone)]
pub struct LiteralExpr {
    pub kind: LiteralKind,
    /// Source-side parse anomaly (overflow, precision loss, …). `kind`
    /// still carries a best-effort value; the analyzer reads this field
    /// to emit warnings / errors.
    pub parse_issue: Option<ParseIssue>,
    pub byte_range: Span,
}

/// Typed literal value, parsed once at lowering. `null` / `this` are
/// keyword tokens, not literals — see [`Expr::Null`] / [`Expr::This`].
/// On a parse failure the variant still commits to a best-effort value
/// and the failure is reported via [`LiteralExpr::parse_issue`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiteralKind {
    Int(i64),
    Float(f64),
    Char(char),
    Bool(bool),
    /// Duration in microseconds (GreyCat's canonical `duration` unit).
    /// Sub-µs suffixes (`ns`, `nanosecond`) truncate toward zero.
    Duration(i64),
    /// Time in microseconds since the Unix epoch.
    Time(i64),
    /// ISO-8601 time literal, parsed to µs-since-epoch. Kept distinct
    /// from [`Self::Time`] so the analyzer can run ISO-specific
    /// diagnostics.
    Iso8601(i64),
}

/// Parse anomaly recorded at lowering time. The analyzer emits the
/// user-facing diagnostic from this tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseIssue {
    /// Numeric exceeded its kind's representable range (`Int` → i64,
    /// `Duration` / `Time` / `Iso8601` → i64 µs after scaling). The
    /// value is saturated.
    Overflow,
    /// Float has more significant decimal digits than f64 can hold.
    /// The value is the nearest f64.
    PrecisionLoss,
    /// Unrecognised char escape, malformed ISO-8601 shape, etc. The
    /// value is a placeholder (`'\0'`, `0` µs, …).
    Malformed,
    /// Unknown suffix. The value is fine, but the suffix in unknown (eg. `2year`, `3foo`)
    Suffix,
}

#[derive(Debug, Clone)]
pub struct StringExpr {
    // P17.5
    /// Text fragments and `${expr}` interpolations in source order. A
    /// non-template string is a single [`StringPart::Lit`]; templates
    /// alternate `Lit` / `Interp`. Each part keeps its own byte range.
    pub parts: Box<[StringPart]>,
    pub byte_range: Span,
}

impl StringExpr {
    /// Concatenated raw fragments — interpolation parts skipped.
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
    /// Raw text between (or around) interpolations. `byte_range`
    /// covers just the fragment — not the `"` quotes or `${...}`
    /// markers.
    Lit { text: String, byte_range: Span },
    /// A `${expr}` interpolation. `byte_range` covers the whole
    /// `${expr}`.
    Interp { expr: Idx<Expr>, byte_range: Span },
}

/// Named object construction — `Foo { field: value }` (grammar's
/// `object_fields`). `Map { k: v }` keys are arbitrary value
/// expressions; the head type makes that distinction downstream. See
/// [`PositionalObjectExpr`] for `Foo { a, b }`.
#[derive(Debug, Clone)]
pub struct ObjectExpr {
    pub ty: Idx<TypeRef>,
    pub fields: Box<[ObjectField]>,
    pub byte_range: Span,
}

/// Positional object construction — `Foo { a, b }` (grammar's
/// `object_initializers`). Only `Array` (any arity), `node` (≤ 1),
/// and the v7 fixed-shape tuples accept this form; the
/// object-construction validator rejects every other head.
#[derive(Debug, Clone)]
pub struct PositionalObjectExpr {
    pub ty: Idx<TypeRef>,
    pub fields: Box<[Idx<Expr>]>,
    pub byte_range: Span,
}

/// One `name: value` entry of an [`ObjectExpr`]. `name` is a full
/// expr (grammar's `object_field` is `name:_expr ":" value:_expr`):
/// an `Expr::Ident` / `Expr::String` attr name for a classic object,
/// an arbitrary key expr for a `Map`. Consumers decode the attr
/// symbol from the key at resolution time.
#[derive(Debug, Clone)]
pub struct ObjectField {
    pub name: Idx<Expr>,
    pub value: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct MemberExpr {
    pub receiver: Idx<Expr>,
    pub property: PropertyName,
    /// `a?.b` / `a?->b` — optional-chaining. When `a: T?` the chain is
    /// null if `a` is null; when `a: T` it's a no-op.
    pub opt_chaining: Option<Span>,
    /// `a.b?` / `a->b?` — lifts the result to nullable regardless of
    /// the declared field type.
    pub post_optional: Option<Span>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct StaticExpr {
    pub ty: Idx<TypeRef>,
    pub property: PropertyName,
    pub byte_range: Span,
}

/// Property name in a `member_expr` / `arrow_expr` / `static_expr`.
/// Both variants resolve to the same field/method — use
/// [`PropertyName::ident`] unless the syntactic form matters
/// (diagnostics, formatter round-trips).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyName {
    /// `a.b`, `a->b`, `T::b` — bareword identifier property.
    Ident(Idx<Ident>),
    /// `a."b.c"`, `a->"b.c"`, `T::"b.c"` — string-literal property.
    /// `Ident.symbol` is the *decoded* name (no quotes);
    /// `Ident.byte_range` covers the whole `"..."` literal.
    String(Idx<Ident>),
}

impl PropertyName {
    /// The interned ident carrying the property name's text + span.
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
    /// `a?[i]` — null-safe index. When `a: T?` the result lifts to
    /// nullable; when `a: T` it's a no-op.
    pub pre_optional: Option<Span>,
    /// `a[i]?` — lifts the result to nullable regardless.
    pub post_optional: Option<Span>,
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
    /// Recognized but uncategorized operator. Carries the verbatim text.
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
    /// `+x` — identity, returns the operand type unchanged.
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
    /// `*n` — node deref. Returns the inner `T` of a `node<T>` /
    /// `nodeTime<T>` / similar receiver, non-null (the dot form
    /// `n.resolve()` returns `T?`).
    Deref,
}

#[derive(Debug, Clone)]
pub struct LambdaExpr {
    pub params: Box<[Idx<FnParam>]>,
    pub return_type: Option<Idx<TypeRef>>,
    pub body: BlockStmt,
    pub byte_range: Span,
}

/// A syntactic type reference, modelling the grammar's `type_ident`:
/// `(ident "::")* ident <generics>? "?"?`.
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
    /// `true` for a leading `typeof` keyword — the reference is a
    /// *type literal* (the value IS a type). The grammar admits the
    /// keyword in two positions (inside `type_ident` and on the
    /// `fn_param` side); lowering collapses both onto this flag.
    pub typeof_marker: bool,
    pub byte_range: Span,
}

impl TypeRef {
    /// `true` iff this ref carries a module path prefix.
    pub fn is_qualified(&self) -> bool {
        !self.qualifier.is_empty()
    }
}
