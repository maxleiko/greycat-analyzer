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
}

#[derive(Eq, PartialEq, Clone, Copy, Serialize, Deserialize, Default)]
pub struct SpanPos {
    pub line: usize,
    pub column: usize,
}

impl SpanPos {
    #[inline(always)]
    pub const fn new(line: usize, column: usize) -> Self {
        Self {
            line,
            column,
        }
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
        Self::new(p.line as u32, p.column as u32)
    }
}

impl From<&SpanPos> for lsp::Position {
    #[allow(clippy::cast_possible_truncation)]
    #[inline(always)]
    fn from(p: &SpanPos) -> Self {
        Self::new(p.line as u32, p.column as u32)
    }
}

impl From<(usize, usize)> for SpanPos {
    #[inline(always)]
    fn from(p: (usize, usize)) -> Self {
        Self {
            line: p.0,
            column: p.1,
        }
    }
}

impl From<(u32, u32)> for SpanPos {
    #[inline(always)]
    fn from((line, column): (u32, u32)) -> Self {
        Self {
            line: line as usize,
            column: column as usize,
        }
    }
}

impl From<SpanPos> for (usize, usize) {
    #[inline(always)]
    fn from(p: SpanPos) -> Self {
        (p.line, p.column)
    }
}

impl From<&'_ SpanPos> for (usize, usize) {
    #[inline(always)]
    fn from(p: &'_ SpanPos) -> Self {
        (p.line, p.column)
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
