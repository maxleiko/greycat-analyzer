use std::fmt;

use serde::{Deserialize, Serialize};

use crate::span::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn to_source_token<'a>(&'a self, source: &'a str) -> SrcToken<'a> {
        SrcToken {
            token: self,
            source,
        }
    }
}

pub struct SrcToken<'a> {
    token: &'a Token,
    source: &'a str,
}

impl fmt::Display for SrcToken<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.token.kind {
            TokenKind::EolComment => write!(f, "EolComment"),
            TokenKind::DocComment => write!(f, "DocComment"),
            TokenKind::BlockComment => write!(f, "BlockComment"),
            TokenKind::Space(n) => write!(f, "Space({n})"),
            TokenKind::NewLine(n) => write!(f, "NewLine({n})"),
            TokenKind::Abstract => write!(f, "Keyword(abstract)"),
            TokenKind::As => write!(f, "Keyword(as)"),
            TokenKind::At => write!(f, "Keyword(at)"),
            TokenKind::Break => write!(f, "Keyword(break)"),
            TokenKind::Breakpoint => write!(f, "Keyword(breakpoint)"),
            TokenKind::Catch => write!(f, "Keyword(catch)"),
            TokenKind::Continue => write!(f, "Keyword(continue)"),
            TokenKind::Do => write!(f, "Keyword(do)"),
            TokenKind::Else => write!(f, "Keyword(else)"),
            TokenKind::Enum => write!(f, "Keyword(enum)"),
            TokenKind::Extends => write!(f, "Keyword(extends)"),
            TokenKind::False => write!(f, "Keyword(false)"),
            TokenKind::For => write!(f, "Keyword(for)"),
            TokenKind::Fn => write!(f, "Keyword(fn)"),
            TokenKind::If => write!(f, "Keyword(if)"),
            TokenKind::In => write!(f, "Keyword(in)"),
            TokenKind::Is => write!(f, "Keyword(is)"),
            TokenKind::Limit => write!(f, "Keyword(limit)"),
            TokenKind::Native => write!(f, "Keyword(native)"),
            TokenKind::Null => write!(f, "Keyword(null)"),
            TokenKind::NaN => write!(f, "Keyword(nan)"),
            TokenKind::Infinity => write!(f, "Keyword(infinity)"),
            TokenKind::Private => write!(f, "Keyword(private)"),
            TokenKind::Return => write!(f, "Keyword(return)"),
            TokenKind::Sampling => write!(f, "Keyword(sampling)"),
            TokenKind::Skip => write!(f, "Keyword(skip)"),
            TokenKind::Static => write!(f, "Keyword(static)"),
            TokenKind::Task => write!(f, "Keyword(task)"),
            TokenKind::This => write!(f, "Keyword(this)"),
            TokenKind::Throw => write!(f, "Keyword(throw)"),
            TokenKind::Try => write!(f, "Keyword(try)"),
            TokenKind::Type => write!(f, "Keyword(type)"),
            TokenKind::True => write!(f, "Keyword(true)"),
            TokenKind::TypeOf => write!(f, "Keyword(typeof)"),
            TokenKind::Use => write!(f, "Keyword(use)"),
            TokenKind::Var => write!(f, "Keyword(var)"),
            TokenKind::While => write!(f, "Keyword(while)"),
            TokenKind::Without => write!(f, "Keyword(without)"),
            TokenKind::Ident => write!(f, "Ident({})", &self.source[self.token.span.as_range()]),
            TokenKind::Number => write!(f, "Number({})", &self.source[self.token.span.as_range()]),
            TokenKind::Char { terminated } => write!(
                f,
                "Char({}, {terminated})",
                &self.source[self.token.span.as_range()]
            ),
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
            TokenKind::AtSign => write!(f, "AtSign"),
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
            TokenKind::DoubleQuote => write!(f, "Doublequote"),
            TokenKind::EnterInterpolation => write!(f, "EnterInterpolation"),
            TokenKind::ExitInterpolation => write!(f, "ExitInterpolation"),
            TokenKind::RawString => write!(f, "RawString"),
            TokenKind::Unknown => write!(f, "<UNKNOWN>"),
            TokenKind::Eof => write!(f, "Eof"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScientificNotation {
    Positive(u8),
    Negative(u8),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum TokenKind {
    /// `// comment`
    EolComment,
    /// `/// doc comment`
    DocComment,
    /// `/* block comment */`
    BlockComment,
    /// Any whitespace characters sequence.
    /// The inner value represents the number of whitespaces.
    Space(u32),
    /// Any newline characters sequence.
    /// The inner value represents the number of newlines.
    NewLine(u32),
    /// "ident" or "continue", ...
    /// At this step keywords are also considered identifiers.
    Ident,
    /// `12_u8`, `3.14`, `1.7976931348623157e+308_f
    Number,
    /// `'c'`, `'😺'`
    Char {
        terminated: bool,
    },
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
    AtSign,
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
    DoubleQuote,
    /// `${`
    EnterInterpolation,
    /// `}`
    ExitInterpolation,
    /// string chunk in a template
    RawString,

    // Keywords
    Abstract,
    As,
    At,
    Break,
    Breakpoint,
    Catch,
    Continue,
    Do,
    Else,
    Enum,
    Extends,
    False,
    For,
    Fn,
    If,
    In,
    Is,
    Limit,
    Native,
    Null,
    NaN,
    Infinity,
    Private,
    Return,
    Sampling,
    Skip,
    Static,
    Task,
    This,
    Throw,
    Try,
    Type,
    True,
    TypeOf,
    Use,
    Var,
    While,
    Without,

    /// Unknown token, not expected by the lexer, e.g. "№"
    Unknown,

    /// End-of-file
    Eof,
}

impl TokenKind {
    pub fn precedence(&self) -> u8 {
        match self {
            Self::Caret => 13,
            Self::Slash | Self::Star | Self::Percent => 12,
            Self::Plus | Self::Minus => 11,
            Self::Gt | Self::GtEq | Self::Lt | Self::LtEq => 9,
            Self::EqEq | Self::BangEq => 8,
            Self::As | Self::Is => 5,
            Self::AndAnd => 4,
            Self::OrOr => 3,
            Self::Eq | Self::QuestionEq => 2,
            _ => 15,
        }
    }
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EolComment => write!(f, "<eol_comment>"),
            Self::DocComment => write!(f, "<doc_comment>"),
            Self::BlockComment => write!(f, "<block_comment>"),
            Self::Space(_) => write!(f, " "),
            Self::NewLine(_) => write!(f, "\\n"),
            Self::Ident => write!(f, "identifier"),
            Self::Abstract => write!(f, "Abstract"),
            Self::As => write!(f, "As"),
            Self::At => write!(f, "At"),
            Self::Break => write!(f, "Break"),
            Self::Breakpoint => write!(f, "Breakpoint"),
            Self::Catch => write!(f, "Catch"),
            Self::Continue => write!(f, "Continue"),
            Self::Do => write!(f, "Do"),
            Self::Else => write!(f, "Else"),
            Self::Enum => write!(f, "Enum"),
            Self::Extends => write!(f, "Extends"),
            Self::False => write!(f, "False"),
            Self::For => write!(f, "For"),
            Self::Fn => write!(f, "Fn"),
            Self::If => write!(f, "If"),
            Self::In => write!(f, "In"),
            Self::Is => write!(f, "Is"),
            Self::Limit => write!(f, "Limit"),
            Self::Native => write!(f, "Native"),
            Self::Null => write!(f, "Null"),
            Self::NaN => write!(f, "NaN"),
            Self::Infinity => write!(f, "Infinity"),
            Self::Private => write!(f, "Private"),
            Self::Return => write!(f, "Return"),
            Self::Sampling => write!(f, "Sampling"),
            Self::Skip => write!(f, "Skip"),
            Self::Static => write!(f, "Static"),
            Self::Task => write!(f, "Task"),
            Self::This => write!(f, "This"),
            Self::Throw => write!(f, "Throw"),
            Self::Try => write!(f, "Try"),
            Self::Type => write!(f, "Type"),
            Self::True => write!(f, "True"),
            Self::TypeOf => write!(f, "TypeOf"),
            Self::Use => write!(f, "Use"),
            Self::Var => write!(f, "Var"),
            Self::While => write!(f, "While"),
            Self::Without => write!(f, "Without"),
            Self::Number => write!(f, "number"),
            Self::Char { .. } => write!(f, "char"),
            Self::Semi => write!(f, ";"),
            Self::Comma => write!(f, ","),
            Self::Dot => write!(f, "."),
            Self::DotDot => write!(f, ".."),
            Self::OpenParen => write!(f, "("),
            Self::CloseParen => write!(f, ")"),
            Self::OpenCurly => write!(f, "{{"),
            Self::CloseCurly => write!(f, "}}"),
            Self::OpenSquare => write!(f, "["),
            Self::CloseSquare => write!(f, "]"),
            Self::AtSign => write!(f, "@"),
            Self::Question => write!(f, "?"),
            Self::QuestionEq => write!(f, "?="),
            Self::QuestionQuestion => write!(f, "??"),
            Self::Colon => write!(f, ":"),
            Self::ColonColon => write!(f, "::"),
            Self::Eq => write!(f, "="),
            Self::EqEq => write!(f, "=="),
            Self::Bang => write!(f, "!"),
            Self::BangEq => write!(f, "!="),
            Self::BangBang => write!(f, "!!"),
            Self::Lt => write!(f, "<"),
            Self::LtEq => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::GtEq => write!(f, ">="),
            Self::Minus => write!(f, "-"),
            Self::MinusMinus => write!(f, "--"),
            Self::Arrow => write!(f, "->"),
            Self::AndAnd => write!(f, "&&"),
            Self::OrOr => write!(f, "||"),
            Self::Plus => write!(f, "+"),
            Self::PlusPlus => write!(f, "++"),
            Self::Star => write!(f, "*"),
            Self::Slash => write!(f, "/"),
            Self::Caret => write!(f, "^"),
            Self::Percent => write!(f, "%"),
            Self::DoubleQuote => write!(f, "\""),
            Self::EnterInterpolation => write!(f, "${{"),
            Self::ExitInterpolation => write!(f, "}}"),
            Self::RawString => write!(f, "<raw_string>"),
            Self::Unknown => write!(f, "<unknown>"),
            Self::Eof => write!(f, "<eof>"),
        }
    }
}

impl TokenKind {
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            TokenKind::EolComment
                | TokenKind::Space(_)
                | TokenKind::NewLine(_)
                | TokenKind::BlockComment
        )
    }
}
