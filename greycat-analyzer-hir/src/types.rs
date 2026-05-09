//! HIR node types — declarations, statements, expressions, type refs.
//! "Type ref" here means *syntactic* type annotation (e.g. `Array<int>`),
//! distinct from the *semantic* `Type` enum that `greycat-analyzer-types`
//! computes during inference.

use std::ops::Range;

use crate::arena::Idx;

pub type Span = Range<usize>;

/// The whole HIR for a single source file. All `Idx<…>` handles in this
/// module index into one of the arenas held by [`crate::Hir`].
#[derive(Debug, Clone)]
pub struct Module {
    pub name: String,
    pub lib: String,
    pub decls: Vec<Idx<Decl>>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct Ident {
    pub text: String,
    pub byte_range: Span,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Modifiers {
    pub private: bool,
    pub static_: bool,
    pub abstract_: bool,
    pub native: bool,
    /// Annotations declared on this decl, drawn from grammar
    /// `annotations`. P13.4: each entry carries the annotation name
    /// plus any string-literal arguments (e.g.
    /// `@expose("renamed")` → `Annotation { name: "expose",
    /// args: ["renamed"] }`). Non-string arguments are dropped —
    /// the consumers we have today (`@expose` rename capture, the
    /// `unused-decl` exposure check) only need string args.
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub name: String,
    pub args: Vec<String>,
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
    pub generics: Vec<Idx<Ident>>,
    pub params: Vec<Idx<FnParam>>,
    pub return_type: Option<Idx<TypeRef>>,
    /// `None` for native / abstract functions.
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
    pub generics: Vec<Idx<Ident>>,
    pub supertype: Option<Idx<TypeRef>>,
    pub attrs: Vec<Idx<TypeAttr>>,
    pub methods: Vec<Idx<Decl>>, // each is a Decl::Fn (FnDecl with `static_` / `abstract_` etc.)
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
    pub fields: Vec<Idx<EnumField>>,
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
    pub args: Vec<Idx<Expr>>,
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
    pub stmts: Vec<Idx<Stmt>>,
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
    pub params: Vec<ForInParam>,
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
    Ident(Idx<Ident>),
    /// Literal whose textual form is preserved verbatim (numbers, durations,
    /// iso8601, char). The semantic `Type` is computed by the type system.
    Literal(LiteralExpr),
    String(StringExpr),
    Tuple(Vec<Idx<Expr>>, Span),
    Array(Vec<Idx<Expr>>, Span),
    Object(ObjectExpr),
    Member(MemberExpr),
    Arrow(MemberExpr), // `n->name` — same shape, different access semantics
    Static(StaticExpr),
    /// P15.8 — chained `module::Type::method` (or longer). The
    /// HIR `StaticExpr` only models `Type::name` because its `ty`
    /// slot is a `TypeRef` and the grammar allows a nested
    /// `static_expr` as the head. For chains the lowering emits
    /// this flat-`Vec<Idx<Ident>>` shape instead. Each segment is
    /// an `ident` from the source. Length is always >= 2.
    QualifiedStatic {
        chain: Vec<Idx<Ident>>,
        byte_range: Span,
    },
    Offset(OffsetExpr),
    Call(CallExpr),
    Binary(BinaryExpr),
    Unary(UnaryExpr),
    Paren(Idx<Expr>, Span),
    Lambda(LambdaExpr),
    /// `value is Type` — runtime type guard, evaluates to `bool`.
    /// Recognized by the analyzer to narrow `value` in the matching
    /// branch when used inside an `if` condition (P6.5).
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
    /// passes can still gracefully skip. Will shrink as P2.3-P2.5 demand
    /// more precise variants.
    Unsupported {
        kind: &'static str,
        byte_range: Span,
    },
}

impl Expr {
    pub fn byte_range(&self) -> Span {
        match self {
            Expr::Ident(_) => 0..0, // resolved via the Ident arena entry
            Expr::Literal(l) => l.byte_range.clone(),
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
            Expr::Unsupported { byte_range, .. } => byte_range.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiteralExpr {
    pub kind: LiteralKind,
    pub text: String,
    pub byte_range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralKind {
    Number,
    Char,
    Bool,
    Null,
    This,
    Duration,
    /// P13.3 — typed-suffix `123_time` literals. Distinct from
    /// [`Self::Iso8601`], which only covers ISO-8601 string-shaped
    /// time literals.
    Time,
    Iso8601,
}

#[derive(Debug, Clone)]
pub struct StringExpr {
    /// P17.5 — `parts` carries the lowered text fragments and
    /// `${expr}` interpolation expressions in source order. A
    /// non-template string (no `${…}`) lowers to a single
    /// `StringPart::Lit` covering the inner text. Template strings
    /// lower to alternating `Lit` / `Interp` entries. Each part keeps
    /// its own byte range so the parity oracle / capabilities can
    /// emit per-fragment records (`RawStringExpr` /
    /// `InterpolationExpr`) and the resolver / analyzer can recurse
    /// into each `Interp.expr`.
    pub parts: Vec<StringPart>,
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

/// P17.5 — one piece of a [`StringExpr`].
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
    pub fields: Vec<ObjectField>,
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
    pub property: Idx<Ident>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct StaticExpr {
    pub ty: Idx<TypeRef>,
    pub property: Idx<Ident>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct OffsetExpr {
    pub receiver: Idx<Expr>,
    pub index: Idx<Expr>,
    pub byte_range: Span,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: Idx<Expr>,
    pub args: Vec<Idx<Expr>>,
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
    Not,
    BitNot,
    NonNullAssert, // !!
}

#[derive(Debug, Clone)]
pub struct LambdaExpr {
    pub params: Vec<Idx<FnParam>>,
    pub body: Idx<Expr>,
    pub byte_range: Span,
}

// =============================================================================
// Type references (syntactic)
// =============================================================================

#[derive(Debug, Clone)]
pub struct TypeRef {
    pub name: Idx<Ident>,
    pub params: Vec<Idx<TypeRef>>,
    pub optional: bool,
    pub byte_range: Span,
}
