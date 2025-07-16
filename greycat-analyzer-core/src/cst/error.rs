use serde::Serialize;

use crate::{
    lexer::{Token, TokenKind},
    span::Span,
};

#[derive(Debug, Serialize)]
pub enum ParseError {
    NoMatch,
    UnexpectedToken(TokenKind, Token),
    ExpectedIdent(String, Token),
    ExpectedIdents(Box<[String]>, Token),
    Unexpected(Span),
    UnexpectedEof,
}

impl ParseError {
    pub fn to_source_error(self, source: &str) -> SourceParseErrorOwned {
        SourceParseErrorOwned {
            source: source.to_string().into_boxed_str(),
            error: self,
        }
    }

    pub fn as_source_error<'a>(&'a self, source: &'a str) -> SourceParseError<'a> {
        SourceParseError {
            source,
            error: self,
        }
    }
}

impl std::error::Error for ParseError {}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO proper display for parse error
        std::fmt::Debug::fmt(self, f)
    }
}

#[derive(Debug, Serialize)]
pub struct SourceParseErrorOwned {
    source: Box<str>,
    error: ParseError,
}

impl std::error::Error for SourceParseErrorOwned {}

impl std::fmt::Display for SourceParseErrorOwned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        SourceParseError {
            source: &self.source,
            error: &self.error,
        }
        .fmt(f)
    }
}

#[derive(Debug)]
pub struct SourceParseError<'a> {
    source: &'a str,
    error: &'a ParseError,
}

impl std::error::Error for SourceParseError<'_> {}

impl std::fmt::Display for SourceParseError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.error {
            ParseError::NoMatch => writeln!(f, "NoMatch"),
            ParseError::UnexpectedToken(expected, got) => writeln!(
                f,
                "expected '{expected:?}', got '{}' at {}",
                &self.source[got.span.as_range(self.source)],
                got.span
            ),
            ParseError::ExpectedIdent(ident, got) => writeln!(
                f,
                "expected '{ident}', got '{}' at {}",
                &self.source[got.span.as_range(self.source)],
                got.span
            ),
            ParseError::ExpectedIdents(idents, got) => writeln!(
                f,
                "expected one of {}, got '{}' at {}",
                idents.join(", "),
                &self.source[got.span.as_range(self.source)],
                got.span
            ),
            ParseError::Unexpected(span) => writeln!(
                f,
                "unexpected token '{}' at {}",
                &self.source[span.as_range(self.source)],
                span
            ),
            ParseError::UnexpectedEof => writeln!(f, "<EOF>"),
        }
    }
}
