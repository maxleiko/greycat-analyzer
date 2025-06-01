mod token;

pub use token::*;

use std::str::Chars;

use crate::span::{Pos, Span};

const EOF: char = '\0';

/// Collects all tokens of the given `source` into a `Vec` using `Lexer`
pub fn tokenize(source: &str) -> Vec<Token> {
    Lexer::new(source).collect()
}

/// `Lexer` implements `Iterator` and is cheap to clone
#[derive(Clone)]
pub struct Lexer<'a> {
    source: &'a str,
    chars: Chars<'a>,
    start: InternalPos,
    curr: InternalPos,
    state: State,
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.next_token()
    }
}

trait Consume<'a> {
    fn consume(&mut self, ctx: &mut Lexer<'a>) -> Token;
}

#[derive(Clone, Copy, Debug)]
enum Consumer {
    Main(MainLexer),
    Template(TemplateLexer),
    Interpolation(InterpolationLexer),
}

#[derive(Clone)]
enum Transition {
    Pop,
    Push(Consumer),
}

#[derive(Clone)]
struct State {
    current: Option<Consumer>,
    stack: Vec<Consumer>,
    next: Option<Transition>,
}

impl State {
    fn transition(&mut self, transition: Transition) {
        self.next = Some(transition);
    }
}

impl<'a> Consume<'a> for Consumer {
    fn consume(&mut self, ctx: &mut Lexer<'a>) -> Token {
        match self {
            Consumer::Main(lexer) => lexer.consume(ctx),
            Consumer::Template(lexer) => lexer.consume(ctx),
            Consumer::Interpolation(lexer) => lexer.consume(ctx),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MainLexer;

impl<'a> Consume<'a> for MainLexer {
    fn consume(&mut self, ctx: &mut Lexer<'a>) -> Token {
        match ctx.next_char() {
            EOF => ctx.token(TokenKind::Eof),
            '%' => ctx.token(TokenKind::Percent),
            '*' => ctx.token(TokenKind::Star),
            '@' => ctx.token(TokenKind::At),
            '{' => ctx.token(TokenKind::OpenCurly),
            '}' => ctx.token(TokenKind::CloseCurly),
            '[' => ctx.token(TokenKind::OpenSquare),
            ']' => ctx.token(TokenKind::CloseSquare),
            '(' => ctx.token(TokenKind::OpenParen),
            ')' => ctx.token(TokenKind::CloseParen),
            '<' if ctx.peek_char(0) == '=' => {
                ctx.next_char(); // consume '='
                ctx.token(TokenKind::LtEq)
            }
            '<' => ctx.token(TokenKind::Lt),
            '>' if ctx.peek_char(0) == '=' => {
                ctx.next_char(); // consume '='
                ctx.token(TokenKind::GtEq)
            }
            '>' => ctx.token(TokenKind::Gt),
            '!' if ctx.peek_char(0) == '=' => {
                ctx.next_char(); // consume '='
                ctx.token(TokenKind::BangEq)
            }
            '!' if ctx.peek_char(0) == '!' => {
                ctx.next_char(); // consume '!'
                ctx.token(TokenKind::BangBang)
            }
            '!' => ctx.token(TokenKind::Bang),
            '?' if ctx.peek_char(0) == '?' => {
                ctx.next_char(); // consume '?'
                ctx.token(TokenKind::QuestionQuestion)
            }
            '?' if ctx.peek_char(0) == '=' => {
                ctx.next_char(); // consume '='
                ctx.token(TokenKind::QuestionEq)
            }
            '?' => ctx.token(TokenKind::Question),
            ':' if ctx.peek_char(0) == ':' => {
                ctx.next_char(); // consume ':'
                ctx.token(TokenKind::ColonColon)
            }
            ':' => ctx.token(TokenKind::Colon),
            '.' if ctx.peek_char(0) == '.' => {
                ctx.next_char(); // consume '.'
                ctx.token(TokenKind::DotDot)
            }
            '.' => ctx.token(TokenKind::Dot),
            ',' => ctx.token(TokenKind::Comma),
            ';' => ctx.token(TokenKind::Semi),
            '+' if ctx.peek_char(0) == '+' => {
                ctx.next_char(); // consume '+'
                ctx.token(TokenKind::PlusPlus)
            }
            '+' => ctx.token(TokenKind::Plus),
            '-' if ctx.peek_char(0) == '-' => {
                ctx.next_char(); // consume '-'
                ctx.token(TokenKind::MinusMinus)
            }
            '-' if ctx.peek_char(0) == '>' => {
                ctx.next_char(); // consume '>'
                ctx.token(TokenKind::Arrow)
            }
            '-' => ctx.token(TokenKind::Minus),
            '=' if ctx.peek_char(0) == '=' => {
                ctx.next_char(); // consume '='
                ctx.token(TokenKind::EqEq)
            }
            '=' => ctx.token(TokenKind::Eq),
            '^' => ctx.token(TokenKind::Caret),
            '&' if ctx.peek_char(0) == '&' => {
                ctx.next_char(); // consume '&'
                ctx.token(TokenKind::AndAnd)
            }
            '|' if ctx.peek_char(0) == '|' => {
                ctx.next_char(); // consume '|'
                ctx.token(TokenKind::OrOr)
            }
            '\n' => {
                let mut count: usize = 1;
                loop {
                    match (ctx.peek_char(0), ctx.peek_char(1)) {
                        ('\r', '\n') => {
                            ctx.next_char(); // consume '\r'
                            ctx.next_char(); // consume '\n'
                            count += 1;
                        }
                        ('\n', _) => {
                            ctx.next_char(); // consume '\n'
                            count += 1;
                        }
                        _ => break,
                    }
                }
                ctx.token(TokenKind::NewLine(count))
            }
            '/' if ctx.peek_char(0) == '/' => {
                if ctx.peek_char(1) == '/' {
                    ctx.next_char(); // consume first '/'
                    ctx.next_char(); // consume second '/'
                    ctx.advance_while(not_newline);
                    ctx.token(TokenKind::DocComment)
                } else {
                    ctx.next_char(); // consume '/'
                    ctx.advance_while(not_newline);
                    ctx.token(TokenKind::EolComment)
                }
            }
            '/' if ctx.peek_char(0) == '*' => {
                ctx.next_char(); // consume '*'
                loop {
                    let c0 = ctx.peek_char(0);
                    let c1 = ctx.peek_char(1);
                    let c2 = ctx.peek_char(2);
                    match (c0, c1, c2) {
                        (EOF, _, _) => {
                            return ctx.token(TokenKind::Eof);
                        }
                        ('\\', c, _) if c != EOF => {
                            ctx.next_char();
                            ctx.next_char();
                        }
                        ('*', '/', _) => {
                            ctx.next_char();
                            ctx.next_char();
                            break;
                        }
                        (_, '*', '/') => {
                            ctx.next_char();
                            ctx.next_char();
                            ctx.next_char();
                            break;
                        }
                        _ => {
                            ctx.next_char();
                        }
                    }
                }
                ctx.token(TokenKind::BlockComment)
            }
            '/' => ctx.token(TokenKind::Slash),
            '\'' => {
                loop {
                    match ctx.next_char() {
                        EOF | '\n' => {
                            return ctx.token(TokenKind::Char { terminated: false });
                        }
                        '\\' => {
                            ctx.next_char(); // skip escaped char
                        }
                        '\'' => {
                            break ctx.token(TokenKind::Char { terminated: true });
                        }
                        _ => (),
                    }
                }
            }
            '"' => {
                ctx.state
                    .transition(Transition::Push(Consumer::Template(TemplateLexer)));
                ctx.token(TokenKind::DoubleQuote)
            }
            c if is_whitespace(c) => {
                ctx.advance_while(is_whitespace);
                ctx.token(TokenKind::Space(
                    ctx.curr.offset as usize - ctx.start.offset as usize,
                ))
            }
            _c @ '0'..='9' => {
                let mut floating = false;
                let mut scientific = None;

                loop {
                    ctx.advance_while(is_number_continue);
                    if ctx.peek_char(0) == '.' {
                        if floating {
                            // already got a '.', moving on
                            break;
                        }
                        floating = true;
                        ctx.next_char(); // consume '.'
                        continue;
                    }
                    break;
                }

                let number_end_offset = ctx.curr.offset;

                // consume potential postfix
                if is_id_start(ctx.peek_char(0)) {
                    let postfix_start = ctx.curr.offset as usize;
                    ctx.next_char(); // consume start of identifier
                    ctx.advance_while(|c| c.is_alphabetic());
                    match &ctx.source[postfix_start..ctx.curr.offset as usize] {
                        "f" | "float" => floating = true,
                        "e" | "E" => match ctx.peek_char(0) {
                            '-' => {
                                ctx.next_char();
                                scientific = Some(ScientificNotation::Negative);
                            }
                            '+' => {
                                ctx.next_char();
                                scientific = Some(ScientificNotation::Positive);
                            }
                            _ => {
                                scientific = Some(ScientificNotation::Positive);
                            }
                        },
                        _ => (),
                    }

                    if scientific.is_some() && ctx.peek_char(0).is_ascii_digit() {
                        ctx.next_char();
                        ctx.advance_while(is_number_continue);
                    }
                }

                let image = ctx.source[ctx.start.offset as usize..number_end_offset as usize]
                    .replace('_', "");
                if floating {
                    let token = if scientific.is_some() {
                        TokenKind::Float { terminated: true }
                    } else {
                        TokenKind::Float {
                            terminated: !image.ends_with('.'),
                        }
                    };
                    ctx.token(token)
                } else {
                    let token = if let Some(s) = scientific {
                        match s {
                            ScientificNotation::Positive => TokenKind::Int,
                            ScientificNotation::Negative => TokenKind::Float { terminated: true },
                        }
                    } else {
                        TokenKind::Int
                    };

                    ctx.token(token)
                }
            }
            c if is_id_start(c) => {
                ctx.advance_while(is_id_continue);
                match ctx.image() {
                    "true" => ctx.token(TokenKind::Bool),
                    "false" => ctx.token(TokenKind::Bool),
                    // id if id_is_keyword(id) => ctx.push_token(TokenKind::Keyword()),
                    _ => ctx.token(TokenKind::Ident),
                }
            }
            _ => ctx.token(TokenKind::Unknown),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TemplateLexer;

impl<'a> Consume<'a> for TemplateLexer {
    fn consume(&mut self, ctx: &mut Lexer<'a>) -> Token {
        match ctx.next_char() {
            EOF => ctx.token(TokenKind::Eof),
            '"' => {
                ctx.state.transition(Transition::Pop);
                ctx.token(TokenKind::DoubleQuote)
            }
            '\\' => {
                // escape skips next char
                ctx.next_char();
                self.consume(ctx)
            }
            '$' if ctx.peek_char(0) == '{' => {
                ctx.next_char(); // consume '{' too
                ctx.state
                    .transition(Transition::Push(Consumer::Interpolation(
                        InterpolationLexer::default(),
                    )));
                ctx.token(TokenKind::EnterInterpolation)
            }
            _ if ctx.peek_char(0) == '"'
                || (ctx.peek_char(0) == '$' && ctx.peek_char(1) == '{')
                || ctx.peek_char(0) == EOF =>
            {
                // if we are about to exit the template or about to enter an interpolation
                // push a new token with the current raw string content
                ctx.token(TokenKind::RawString)
            }
            _ => {
                // keep on consuming
                self.consume(ctx)
            }
        }
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct InterpolationLexer {
    curly_depth: usize,
}

impl<'a> Consume<'a> for InterpolationLexer {
    /// Delegates consumes to MainLexer, but keeps track of OpenCurly/CloseCurly depth
    fn consume(&mut self, ctx: &mut Lexer<'a>) -> Token {
        let mut main_lexer = MainLexer;

        match ctx.peek_char(0) {
            EOF => main_lexer.consume(ctx),
            '{' => {
                self.curly_depth += 1;
                main_lexer.consume(ctx)
            }
            '}' => {
                if self.curly_depth == 0 {
                    ctx.next_char(); // consume '}'
                    ctx.state.transition(Transition::Pop);
                    return ctx.token(TokenKind::ExitInterpolation);
                }
                self.curly_depth -= 1;
                main_lexer.consume(ctx)
            }
            _ => main_lexer.consume(ctx),
        }
    }
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            chars: source.chars(),
            start: InternalPos::default(),
            curr: InternalPos::default(),
            state: State {
                current: Some(Consumer::Main(MainLexer)),
                stack: Vec::with_capacity(15),
                next: None,
            },
        }
    }

    /// Returns a vector of tokens from the current lexer source.
    ///
    /// This is equivalent to `lexer.collect::<Vec<Token<'_>>>()`.
    ///
    /// *This method takes ownership of `self` because once the source
    /// is tokenized, we are at the end of the source, therefore no more
    /// tokens can be produced.*
    #[inline(always)]
    pub fn tokenize(self) -> Vec<Token> {
        self.collect()
    }

    pub fn next_token(&mut self) -> Option<Token> {
        if self.curr.offset as usize == self.source.len() {
            return None;
        }

        let mut current_lexer = self
            .state
            .current
            .take()
            // SAFETY:
            // if this is not valid it means something is wrong in the lexer stack
            // which is likely due to a bug introduced by popping too much
            .expect("internal error, no more lexer in stack");
        let token = current_lexer.consume(self);
        match self.state.next.take() {
            Some(transition) => {
                match transition {
                    Transition::Pop => {
                        self.state.current = self.state.stack.pop();
                    }
                    Transition::Push(next) => {
                        if self.state.stack.len() == 15 {
                            // We could handle an infinite stack (as long as we have memory) here
                            // but the current GreyCat compiler won't allow more than 15, so let's
                            // stick to what the compiler knows
                            panic!("internal error, GreyCat only allows 15 nested interpolations");
                        }
                        self.state.current = Some(next);
                        self.state.stack.push(current_lexer);
                    }
                }
            }
            None => self.state.current = Some(current_lexer),
        }
        Some(token)
    }

    #[inline(always)]
    fn next_char(&mut self) -> char {
        match self.chars.next() {
            None => EOF,
            Some(c) => {
                self.curr.increase_by_char_len(c);
                c
            }
        }
    }

    #[inline(always)]
    pub fn peek_char(&self, n: usize) -> char {
        self.chars.clone().nth(n).unwrap_or(EOF)
    }

    fn advance_while(&mut self, predicate: fn(char) -> bool) {
        loop {
            let c = self.peek_char(0);
            if c == EOF || !predicate(c) {
                break;
            }
            self.next_char();
        }
    }

    #[inline(always)]
    fn image(&self) -> &'a str {
        &self.source[self.start.offset as usize..self.curr.offset as usize]
    }

    fn token(&mut self, kind: TokenKind) -> Token {
        let token = Token {
            kind,
            span: Span {
                start: self.start.into(),
                end: self.curr.into(),
            },
        };
        // after a new "token" is pushed, reset "start" position to "curr" position
        self.start = self.curr;

        token
    }
}

#[inline]
fn is_id_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

#[inline]
fn is_id_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[inline]
fn is_number_continue(c: char) -> bool {
    c == '_' || c.is_numeric()
}

#[inline]
fn is_whitespace(c: char) -> bool {
    // whitespace, tab, non-breaking whitespace
    matches!(c, ' ' | '\t' | '\u{A0}')
}

#[inline]
fn not_newline(c: char) -> bool {
    c != '\n'
}

/// An internal struct that knows how to compute line and characters based on the given `c`.
///
#[derive(Default, Clone, Copy, Debug)]
struct InternalPos {
    line: u32,
    characters: u32,
    offset: u32,
}

impl InternalPos {
    fn increase_by_char_len(&mut self, c: char) {
        let len = c.len_utf8() as u32;
        self.characters += len;
        self.offset += len;
        if c == '\n' {
            self.line += 1;
            self.characters = 0;
        }
    }
}

impl From<InternalPos> for Pos {
    fn from(value: InternalPos) -> Self {
        Self::new(value.line, value.characters)
    }
}

enum ScientificNotation {
    Positive,
    Negative,
}

#[cfg(test)]
mod test {
    use crate::span::Pos;

    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn curly_nl() {
        let tokens = tokenize("{\n}");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::OpenCurly,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::NewLine(1),
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(1, 0),
                    }
                },
                Token {
                    kind: TokenKind::CloseCurly,
                    span: Span {
                        start: Pos::new(1, 0),
                        end: Pos::new(1, 1),
                    }
                }
            ]
        );
    }

    #[test]
    fn string_lit() {
        let tokens = tokenize("\"hello world\"");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::RawString,
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(0, 12),
                    }
                },
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 12),
                        end: Pos::new(0, 13),
                    }
                },
            ]
        );
    }

    #[test]
    fn string_lit_unfinished() {
        let tokens = tokenize("\"hello ");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::RawString,
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(0, 7),
                    }
                },
            ]
        );
    }

    #[test]
    fn string_lit_with_interpolation() {
        let tokens = tokenize("\"hello ${world}\"");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::RawString,
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(0, 7),
                    }
                },
                Token {
                    kind: TokenKind::EnterInterpolation,
                    span: Span {
                        start: Pos::new(0, 7),
                        end: Pos::new(0, 9),
                    }
                },
                Token {
                    kind: TokenKind::Ident,
                    span: Span {
                        start: Pos::new(0, 9),
                        end: Pos::new(0, 14),
                    }
                },
                Token {
                    kind: TokenKind::ExitInterpolation,
                    span: Span {
                        start: Pos::new(0, 14),
                        end: Pos::new(0, 15),
                    }
                },
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 15),
                        end: Pos::new(0, 16),
                    }
                },
            ]
        );
    }

    #[test]
    fn string_lit_with_unfinished_interpolation() {
        let tokens = tokenize("\"hello ${world\"");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::RawString,
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(0, 7),
                    }
                },
                Token {
                    kind: TokenKind::EnterInterpolation,
                    span: Span {
                        start: Pos::new(0, 7),
                        end: Pos::new(0, 9),
                    }
                },
                Token {
                    kind: TokenKind::Ident,
                    span: Span {
                        start: Pos::new(0, 9),
                        end: Pos::new(0, 14),
                    }
                },
                Token {
                    kind: TokenKind::DoubleQuote,
                    span: Span {
                        start: Pos::new(0, 14),
                        end: Pos::new(0, 15),
                    }
                },
            ]
        );
    }

    #[test]
    fn int_literal() {
        let tokens = tokenize("42");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Int,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 2),
                }
            }]
        );
    }

    #[test]
    fn float_literal() {
        let tokens = tokenize("3.14");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Float { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 4),
                }
            }]
        );
    }

    #[test]
    fn float_literal_unfinished() {
        let tokens = tokenize("3.");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Float { terminated: false },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 2),
                }
            }]
        );
    }

    #[test]
    fn float_literal_too_many_dots() {
        let tokens = tokenize("3.1.4");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::Float { terminated: true },
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 3),
                    }
                },
                Token {
                    kind: TokenKind::Dot,
                    span: Span {
                        start: Pos::new(0, 3),
                        end: Pos::new(0, 4),
                    }
                },
                Token {
                    kind: TokenKind::Int,
                    span: Span {
                        start: Pos::new(0, 4),
                        end: Pos::new(0, 5),
                    }
                },
            ]
        );
    }

    #[test]
    fn int_literal_with_underscores() {
        let tokens = tokenize("1_000_000");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Int,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 9),
                }
            }]
        );
    }

    #[test]
    fn explicit_float_literal() {
        let tokens = tokenize("3f");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Float { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 2),
                }
            }]
        );
    }

    #[test]
    fn explicit_float_literal2() {
        let tokens = tokenize("3_float");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Float { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 7),
                }
            }]
        );
    }

    #[test]
    fn whitespace() {
        let tokens = tokenize(" \t  ");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Space(4),
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 4),
                }
            }]
        );
    }

    #[test]
    fn newline() {
        let tokens = tokenize("\n\r\n\n");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::NewLine(3),
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(3, 0),
                }
            }]
        );
    }

    #[test]
    fn eol_comment() {
        let tokens = tokenize("// hello");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::EolComment,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 8),
                }
            }]
        );
    }

    #[test]
    fn block_comment() {
        let tokens = tokenize("/* hello /*\n\n * world */");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::BlockComment,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(2, 11),
                }
            }]
        );
    }

    #[test]
    fn block_comment_with_escape() {
        let tokens = tokenize("/* \\* */");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::BlockComment,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 8),
                }
            }]
        );
    }

    #[test]
    fn scientific_notation() {
        let tokens = tokenize("1e6");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Int,
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 3),
                }
            }]
        );
    }

    #[test]
    fn scientific_notation_nagative() {
        let tokens = tokenize("1e-6");
        assert_eq!(
            tokens,
            vec![Token {
                kind: TokenKind::Float { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 4),
                }
            }]
        );
    }

    #[test]
    fn small_binop() {
        let tokens = tokenize("a <= 42");
        assert_eq!(
            tokens,
            vec![
                Token {
                    kind: TokenKind::Ident,
                    span: Span {
                        start: Pos::new(0, 0),
                        end: Pos::new(0, 1),
                    }
                },
                Token {
                    kind: TokenKind::Space(1),
                    span: Span {
                        start: Pos::new(0, 1),
                        end: Pos::new(0, 2),
                    }
                },
                Token {
                    kind: TokenKind::LtEq,
                    span: Span {
                        start: Pos::new(0, 2),
                        end: Pos::new(0, 4),
                    }
                },
                Token {
                    kind: TokenKind::Space(1),
                    span: Span {
                        start: Pos::new(0, 4),
                        end: Pos::new(0, 5),
                    }
                },
                Token {
                    kind: TokenKind::Int,
                    span: Span {
                        start: Pos::new(0, 5),
                        end: Pos::new(0, 7),
                    }
                }
            ]
        );
    }

    #[test]
    fn char() {
        let result = tokenize("'c'");
        assert_eq!(
            result,
            vec![Token {
                kind: TokenKind::Char { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 3),
                }
            }]
        );
    }

    #[test]
    fn char_escape() {
        let result = tokenize(r#"'\\'"#);
        assert_eq!(
            result,
            vec![Token {
                kind: TokenKind::Char { terminated: true },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 4),
                }
            }]
        );
    }

    #[test]
    fn char_unfinished() {
        let result = tokenize(r#"'c"#);
        assert_eq!(
            result,
            vec![Token {
                kind: TokenKind::Char { terminated: false },
                span: Span {
                    start: Pos::new(0, 0),
                    end: Pos::new(0, 2),
                }
            }]
        );
    }
}
