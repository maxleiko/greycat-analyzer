use crate::span::Span;

pub struct Parser<'src> {
    source: &'src str,
    errors: Vec<ParseError>,
}

pub struct ParseError {
    message: String,
    span: Span,
}
