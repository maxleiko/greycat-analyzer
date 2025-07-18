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
            TokenKind::Abstract => write!(f, "keyword 'abstract'"),
            TokenKind::As => write!(f, "keyword 'as'"),
            TokenKind::At => write!(f, "keyword 'at'"),
            TokenKind::Break => write!(f, "keyword 'break'"),
            TokenKind::Breakpoint => write!(f, "keyword 'breakpoint'"),
            TokenKind::Catch => write!(f, "keyword 'catch'"),
            TokenKind::Continue => write!(f, "keyword 'continue'"),
            TokenKind::Do => write!(f, "keyword 'do'"),
            TokenKind::Else => write!(f, "keyword 'else'"),
            TokenKind::Enum => write!(f, "keyword 'enum'"),
            TokenKind::Extends => write!(f, "keyword 'extends'"),
            TokenKind::False => write!(f, "keyword 'false'"),
            TokenKind::For => write!(f, "keyword 'for'"),
            TokenKind::Fn => write!(f, "keyword 'fn'"),
            TokenKind::If => write!(f, "keyword 'if'"),
            TokenKind::In => write!(f, "keyword 'in'"),
            TokenKind::Is => write!(f, "keyword 'is'"),
            TokenKind::Limit => write!(f, "keyword 'limit'"),
            TokenKind::Native => write!(f, "keyword 'native'"),
            TokenKind::Null => write!(f, "keyword 'null'"),
            TokenKind::NaN => write!(f, "keyword 'nan'"),
            TokenKind::Infinity => write!(f, "keyword 'infinity'"),
            TokenKind::Private => write!(f, "keyword 'private'"),
            TokenKind::Return => write!(f, "keyword 'return'"),
            TokenKind::Sampling => write!(f, "keyword 'sampling'"),
            TokenKind::Skip => write!(f, "keyword 'skip'"),
            TokenKind::Static => write!(f, "keyword 'static'"),
            TokenKind::Task => write!(f, "keyword 'task'"),
            TokenKind::This => write!(f, "keyword 'this'"),
            TokenKind::Throw => write!(f, "keyword 'throw'"),
            TokenKind::Try => write!(f, "keyword 'try'"),
            TokenKind::Type => write!(f, "keyword 'type'"),
            TokenKind::True => write!(f, "keyword 'true'"),
            TokenKind::TypeOf => write!(f, "keyword 'typeof'"),
            TokenKind::Use => write!(f, "keyword 'use'"),
            TokenKind::Var => write!(f, "keyword 'var'"),
            TokenKind::While => write!(f, "keyword 'while'"),
            TokenKind::Without => write!(f, "keyword 'without'"),
            TokenKind::Ident => write!(f, "Ident({})", &self.source[self.token.span.as_range()]),
            TokenKind::Int => write!(f, "Int({})", &self.source[self.token.span.as_range()]),
            TokenKind::Float { terminated } => write!(
                f,
                "Float({}, {terminated})",
                &self.source[self.token.span.as_range()]
            ),
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
            TokenKind::Eof => write!(f, "<EOF>"),
        }
    }
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
    Float {
        terminated: bool,
    },
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
            Self::Int => write!(f, "int"),
            Self::Float { terminated } => write!(f, "float"),
            Self::Char { terminated } => write!(f, "char"),
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
