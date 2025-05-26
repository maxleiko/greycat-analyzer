use std::ops::Range;

use lsp_types as lsp;
use serde::{Deserialize, Serialize};

pub trait ToSpan {
    fn span(&self) -> Span;
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize, Default)]
pub struct Span {
    pub start: SpanPos,
    pub end: SpanPos,
}

impl Span {
    #[inline(always)]
    pub const fn new(start: SpanPos, end: SpanPos) -> Self {
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

    pub fn as_str<'s>(&self, source: &'s str) -> &'s str {
        &source[self.as_range(source)]
    }
}

#[derive(Eq, PartialEq, Clone, Copy, Serialize, Deserialize, Default)]
pub struct SpanPos {
    pub line: u32,
    pub column: u32,
}

impl SpanPos {
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

impl From<(SpanPos, SpanPos)> for Span {
    #[inline(always)]
    fn from((start, end): (SpanPos, SpanPos)) -> Self {
        Self::new(start, end)
    }
}

impl From<SpanPos> for lsp::Range {
    #[inline(always)]
    fn from(s: SpanPos) -> Self {
        Self::new(s.into(), s.into())
    }
}

impl From<&SpanPos> for lsp::Range {
    #[inline(always)]
    fn from(s: &SpanPos) -> Self {
        Self::new(s.into(), s.into())
    }
}

impl From<SpanPos> for lsp::Position {
    #[allow(clippy::cast_possible_truncation)]
    #[inline(always)]
    fn from(p: SpanPos) -> Self {
        Self {
            line: p.line,
            character: p.column,
        }
    }
}

impl From<&SpanPos> for lsp::Position {
    #[allow(clippy::cast_possible_truncation)]
    #[inline(always)]
    fn from(p: &SpanPos) -> Self {
        Self {
            line: p.line,
            character: p.column,
        }
    }
}

impl From<(usize, usize)> for SpanPos {
    #[inline(always)]
    fn from(p: (usize, usize)) -> Self {
        Self {
            line: p.0 as u32,
            column: p.1 as u32,
        }
    }
}

impl From<(u32, u32)> for SpanPos {
    #[inline(always)]
    fn from((line, column): (u32, u32)) -> Self {
        Self { line, column }
    }
}

impl From<SpanPos> for (usize, usize) {
    #[inline(always)]
    fn from(p: SpanPos) -> Self {
        (p.line as usize, p.column as usize)
    }
}

impl From<&'_ SpanPos> for (usize, usize) {
    #[inline(always)]
    fn from(p: &'_ SpanPos) -> Self {
        (p.line as usize, p.column as usize)
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.start.line, self.start.column)
    }
}

impl std::fmt::Debug for SpanPos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:4}:{:<4}", self.line, self.column)
    }
}

impl std::fmt::Display for SpanPos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

#[test]
fn span_as_range_total() {
    let span = Span {
        start: SpanPos { line: 0, column: 0 },
        end: SpanPos { line: 0, column: 5 },
    };
    let source = "hello";
    assert_eq!(&source[span.as_range(source)], "hello");
}

#[test]
fn span_as_range_partial_start() {
    let span = Span {
        start: SpanPos { line: 0, column: 0 },
        end: SpanPos { line: 0, column: 5 },
    };
    let source = "hello world";
    assert_eq!(&source[span.as_range(source)], "hello");
}

#[test]
fn span_as_range_partial_end() {
    let span = Span {
        start: SpanPos { line: 0, column: 6 },
        end: SpanPos {
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
        start: SpanPos { line: 1, column: 0 },
        end: SpanPos { line: 1, column: 3 },
    };
    let source = "one\ntwo\nthree";
    assert_eq!(&source[span.as_range(source)], "two");
}

#[test]
fn span_as_range_multiline() {
    let span = Span {
        start: SpanPos { line: 3, column: 2 },
        end: SpanPos { line: 4, column: 1 },
    };
    let source = "one\n\ntwo\nthree\nfour\n\nfive\nsix";
    assert_eq!(&source[span.as_range(source)], "ree\nf");
}
