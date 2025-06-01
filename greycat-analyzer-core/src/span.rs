use std::ops::Range;

use lsp_types as lsp;
use serde::{Deserialize, Serialize, ser::SerializeTuple};

pub trait ToSpan {
    fn span(&self) -> Span;
}

#[derive(
    Debug, Eq, Hash, PartialEq, Clone, Copy, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    #[inline(always)]
    pub const fn new(start: Pos, end: Pos) -> Self {
        Self { start, end }
    }

    pub fn as_range(&self, source: &str) -> Range<usize> {
        let start = self.start.offset_in(source);
        let end = if self.start.line == self.end.line {
            start + (self.end.column - self.start.column)
        } else {
            start
                + source[start as usize..]
                    .lines()
                    .take((self.end.line - self.start.line) as usize)
                    .fold(0, |acc, line| acc + line.len() + 1) as u32
                + self.end.column
        };
        Range {
            start: start as usize,
            end: end as usize,
        }
    }

    pub fn to_range(&self) -> lsp::Range {
        lsp::Range {
            start: self.start.to_position(),
            end: self.end.to_position(),
        }
    }

    pub fn as_str<'s>(&self, source: &'s str) -> &'s str {
        &source[self.as_range(source)]
    }
}

#[derive(Eq, Hash, PartialEq, Clone, Copy, PartialOrd, Ord, Deserialize, Default, Debug)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
}

impl Pos {
    #[inline(always)]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// Returns the absolute offset of this position in the given source
    pub fn offset_in(&self, source: &str) -> u32 {
        source
            .lines()
            .take(self.line as usize)
            .fold(0, |acc, line| acc + line.len() + 1) as u32
            + self.column
    }

    pub fn to_position(&self) -> lsp::Position {
        lsp::Position {
            line: self.line,
            character: self.column,
        }
    }
}

impl Serialize for Pos {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&self.line)?;
        tuple.serialize_element(&self.column)?;
        tuple.end()
    }
}

impl From<Span> for lsp::Range {
    #[inline(always)]
    fn from(s: Span) -> Self {
        Self::new(s.start.into(), s.end.into())
    }
}

impl From<&Span> for lsp::Range {
    #[inline(always)]
    fn from(s: &Span) -> Self {
        Self::new(s.start.into(), s.end.into())
    }
}

impl From<(Pos, Pos)> for Span {
    #[inline(always)]
    fn from((start, end): (Pos, Pos)) -> Self {
        Self::new(start, end)
    }
}

impl From<Pos> for lsp::Range {
    #[inline(always)]
    fn from(s: Pos) -> Self {
        Self::new(s.into(), s.into())
    }
}

impl From<&Pos> for lsp::Range {
    #[inline(always)]
    fn from(s: &Pos) -> Self {
        Self::new(s.into(), s.into())
    }
}

impl From<Pos> for lsp::Position {
    #[allow(clippy::cast_possible_truncation)]
    #[inline(always)]
    fn from(p: Pos) -> Self {
        Self {
            line: p.line,
            character: p.column,
        }
    }
}

impl From<&Pos> for lsp::Position {
    #[allow(clippy::cast_possible_truncation)]
    #[inline(always)]
    fn from(p: &Pos) -> Self {
        Self {
            line: p.line,
            character: p.column,
        }
    }
}

impl From<(usize, usize)> for Pos {
    #[inline(always)]
    fn from(p: (usize, usize)) -> Self {
        Self {
            line: p.0 as u32,
            column: p.1 as u32,
        }
    }
}

impl From<(u32, u32)> for Pos {
    #[inline(always)]
    fn from((line, column): (u32, u32)) -> Self {
        Self { line, column }
    }
}

impl From<Pos> for (usize, usize) {
    #[inline(always)]
    fn from(p: Pos) -> Self {
        (p.line as usize, p.column as usize)
    }
}

impl From<&'_ Pos> for (usize, usize) {
    #[inline(always)]
    fn from(p: &'_ Pos) -> Self {
        (p.line as usize, p.column as usize)
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.start.line + 1, self.start.column + 1)
    }
}

impl std::fmt::Display for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

#[test]
fn span_as_range_total() {
    let span = Span {
        start: Pos { line: 0, column: 0 },
        end: Pos { line: 0, column: 5 },
    };
    let source = "hello";
    assert_eq!(&source[span.as_range(source)], "hello");
}

#[test]
fn span_as_range_partial_start() {
    let span = Span {
        start: Pos { line: 0, column: 0 },
        end: Pos { line: 0, column: 5 },
    };
    let source = "hello world";
    assert_eq!(&source[span.as_range(source)], "hello");
}

#[test]
fn span_as_range_partial_end() {
    let span = Span {
        start: Pos { line: 0, column: 6 },
        end: Pos {
            line: 0,
            column: 11,
        },
    };
    let source = "hello world";
    assert_eq!(&source[span.as_range(source)], "world");
}

#[test]
fn span_as_range_not_first_line() {
    let span = Span {
        start: Pos { line: 1, column: 0 },
        end: Pos { line: 1, column: 3 },
    };
    let source = "one\ntwo\nthree";
    assert_eq!(&source[span.as_range(source)], "two");
}

#[test]
fn span_as_range_multiline() {
    let span = Span {
        start: Pos { line: 3, column: 2 },
        end: Pos { line: 4, column: 1 },
    };
    let source = "one\n\ntwo\nthree\nfour\n\nfive\nsix";
    assert_eq!(&source[span.as_range(source)], "ree\nf");
}
