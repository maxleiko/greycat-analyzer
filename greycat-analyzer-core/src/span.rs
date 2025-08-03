use std::ops::Range;

use lsp_types as lsp;
use serde::{Deserialize, Serialize};

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

    pub fn as_range(&self) -> Range<usize> {
        self.start.offset as usize..self.end.offset as usize
    }

    pub fn to_range(&self) -> lsp::Range {
        lsp::Range {
            start: self.start.to_position(),
            end: self.end.to_position(),
        }
    }

    pub fn to_span_str<'src>(&self, source: &'src str) -> SpanStr<'src> {
        SpanStr {
            span: *self,
            image: &source[*self],
        }
    }
}

#[derive(
    Debug, Eq, Hash, PartialEq, Clone, Copy, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct SpanStr<'src> {
    pub span: Span,
    pub image: &'src str,
}

#[derive(
    Eq, Hash, PartialEq, Clone, Copy, PartialOrd, Ord, Serialize, Deserialize, Default, Debug,
)]
pub struct Pos {
    pub line: u32,
    pub column: u32,
    pub offset: u32,
}

impl Pos {
    #[inline(always)]
    pub const fn new(line: u32, column: u32, offset: u32) -> Self {
        Self {
            line,
            column,
            offset,
        }
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
            line: self.line + 1,
            character: self.column + 1,
        }
    }
}

impl std::ops::Index<Span> for str {
    type Output = str;

    fn index(&self, index: Span) -> &Self::Output {
        &self[index.as_range()]
    }
}

impl std::ops::Index<Span> for String {
    type Output = str;

    fn index(&self, index: Span) -> &Self::Output {
        &self[index.as_range()]
    }
}

impl std::ops::Index<&Span> for str {
    type Output = str;

    fn index(&self, index: &Span) -> &Self::Output {
        &self[index.as_range()]
    }
}

impl std::ops::Index<&Span> for String {
    type Output = str;

    fn index(&self, index: &Span) -> &Self::Output {
        &self[index.as_range()]
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

impl From<(usize, usize, usize)> for Pos {
    #[inline(always)]
    fn from((line, column, offset): (usize, usize, usize)) -> Self {
        Self {
            line: line as u32,
            column: column as u32,
            offset: offset as u32,
        }
    }
}

impl From<(u32, u32, u32)> for Pos {
    #[inline(always)]
    fn from((line, column, offset): (u32, u32, u32)) -> Self {
        Self {
            line,
            column,
            offset,
        }
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
    let span = Span::new(Pos::new(0, 0, 0), Pos::new(0, 5, 5));
    let source = "hello";
    assert_eq!(&source[span.as_range()], "hello");
}

#[test]
fn span_as_range_partial_start() {
    let span = Span::new(Pos::new(0, 0, 0), Pos::new(0, 5, 5));
    let source = "hello world";
    assert_eq!(&source[span.as_range()], "hello");
}

#[test]
fn span_as_range_partial_end() {
    let span = Span::new(Pos::new(0, 6, 6), Pos::new(0, 11, 11));
    let source = "hello world";
    assert_eq!(&source[span.as_range()], "world");
}

#[test]
fn span_as_range_not_first_line() {
    let span = Span::new(Pos::new(1, 0, 4), Pos::new(1, 3, 7));
    let source = "one\ntwo\nthree";
    assert_eq!(&source[span.as_range()], "two");
}

#[test]
fn span_as_range_multiline() {
    let span = Span::new(Pos::new(3, 2, 11), Pos::new(4, 1, 16));
    let source = "one\n\ntwo\nthree\nfour\n\nfive\nsix";
    assert_eq!(&source[span.as_range()], "ree\nf");
}
