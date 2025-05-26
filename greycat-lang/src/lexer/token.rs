use std::fmt;

use lsp_types::{Position, Range};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span<'a> {
    /// The actual source string
    pub image: &'a str,
    /// The starting position in the source
    pub start: Position,
    /// The ending position in the source
    pub end: Position,
}

impl<'a> Span<'a> {
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.image.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.image.is_empty()
    }

    pub fn as_range(&self) -> Range {
        Range::new(self.start, self.end)
    }

    pub fn eof() -> Span<'a> {
        Span {
            image: "<eof>",
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: 0,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token<'a> {
    pub kind: TokenKind,
    #[serde(borrow)]
    pub span: Span<'a>,
}

impl fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TokenKind::EolComment => write!(f, "EolComment"),
            TokenKind::DocComment => write!(f, "DocComment"),
            TokenKind::BlockComment => write!(f, "BlockComment"),
            TokenKind::Space(n) => write!(f, "Space({n})"),
            TokenKind::NewLine(n) => write!(f, "NewLine({n})"),
            TokenKind::Ident => write!(f, "Ident({})", self.span.image),
            TokenKind::Int => write!(f, "Int({})", self.span.image),
            TokenKind::Float { terminated } => write!(f, "Float({}, {terminated})", self.span.image),
            TokenKind::Char { terminated } => write!(f, "Char({}, {terminated})", self.span.image),
            TokenKind::Bool => write!(f, "Bool({})", self.span.image),
            TokenKind::Semi => write!(f, "Semi"),
            TokenKind::Comma => write!(f, "Comma"),
            TokenKind::Dot => write!(f, "Dot"),
            TokenKind::DotDot => write!(f, "DotDot"),
            TokenKind::OpenParen => write!(f, "OpenParen"),
            TokenKind::CloseParen => write!(f, "CloseParen"),
            TokenKind::OpenCurly => write!(f, "OpenCurly"),
            TokenKind::CloseCurly => write!(f, "CloseCurly"),
            TokenKind::OpenSquare => write!(f, "OpenSquare"),
            TokenKind::CloseSquare => write!(f, "CloseSquare"),
            TokenKind::At => write!(f, "At"),
            TokenKind::Question => write!(f, "Question"),
            TokenKind::QuestionEq => write!(f, "QuestionEq"),
            TokenKind::QuestionQuestion => write!(f, "QuestionQuestion"),
            TokenKind::Colon => write!(f, "Colon"),
            TokenKind::ColonColon => write!(f, "ColonColon"),
            TokenKind::Eq => write!(f, "Eq"),
            TokenKind::EqEq => write!(f, "EqEq"),
            TokenKind::Bang => write!(f, "Bang"),
            TokenKind::BangEq => write!(f, "BangEq"),
            TokenKind::BangBang => write!(f, "BangBang"),
            TokenKind::Lt => write!(f, "Lt"),
            TokenKind::LtEq => write!(f, "LtEq"),
            TokenKind::Gt => write!(f, "Gt"),
            TokenKind::GtEq => write!(f, "GtEq"),
            TokenKind::Minus => write!(f, "Minus"),
            TokenKind::MinusMinus => write!(f, "MinusMinus"),
            TokenKind::Arrow => write!(f, "Arrow"),
            TokenKind::AndAnd => write!(f, "AndAnd"),
            TokenKind::OrOr => write!(f, "OrOr"),
            TokenKind::Plus => write!(f, "Plus"),
            TokenKind::PlusPlus => write!(f, "PlusPlus"),
            TokenKind::Star => write!(f, "Star"),
            TokenKind::Slash => write!(f, "Slash"),
            TokenKind::Caret => write!(f, "Caret"),
            TokenKind::Percent => write!(f, "Percent"),
            TokenKind::Doublequote => write!(f, "Doublequote"),
            TokenKind::EnterInterpolation => write!(f, "EnterInterpolation"),
            TokenKind::ExitInterpolation => write!(f, "ExitInterpolation"),
            TokenKind::RawString => write!(f, "RawString"),
            TokenKind::Unknown => write!(f, "<UNKNOWN>"),
            TokenKind::Eof => write!(f, "<EOF>>"),
        }
    }
}

impl<'a> From<Token<'a>> for Span<'a> {
    fn from(value: Token<'a>) -> Self {
        value.span
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum TokenKind {
    /// `// comment`
    EolComment,
    /// `/// doc comment`
    DocComment,
    /// `/* block comment */`
    BlockComment,
    /// Any whitespace characters sequence.
    /// The inner value represents the number of whitespaces.
    Space(usize),
    /// Any newline characters sequence.
    /// The inner value represents the number of newlines.
    NewLine(usize),
    /// "ident" or "continue", ...
    /// At this step keywords are also considered identifiers.
    Ident,
    /// `12_u8`
    Int,
    /// `3.14`
    Float { terminated: bool },
    /// `'c'`, `'😺'`
    Char { terminated: bool },
    /// `true`, `false`
    Bool,
    // One-char tokens:
    /// `;`
    Semi,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// `(`
    OpenParen,
    /// `)`
    CloseParen,
    /// `{`
    OpenCurly,
    /// `}`
    CloseCurly,
    /// `[`
    OpenSquare,
    /// `]`
    CloseSquare,
    /// `@`
    At,
    /// `?`
    Question,
    /// `?=`
    QuestionEq,
    /// `??`
    QuestionQuestion,
    /// `:`
    Colon,
    /// `::`
    ColonColon,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `!`
    Bang,
    /// `!=`
    BangEq,
    /// `!!`
    BangBang,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `-`
    Minus,
    /// `--`
    MinusMinus,
    /// `->`
    Arrow,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `+`
    Plus,
    /// `++`
    PlusPlus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `^`
    Caret,
    /// `%`
    Percent,
    /// `"`
    Doublequote,
    /// `${`
    EnterInterpolation,
    /// `}`
    ExitInterpolation,
    /// string chunk in a template
    RawString,

    /// Unknown token, not expected by the lexer, e.g. "№"
    Unknown,

    /// End-of-file
    Eof,
}