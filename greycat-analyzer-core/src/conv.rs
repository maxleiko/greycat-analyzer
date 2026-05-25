//! LSP ↔ byte-offset conversions. LSP positions are 0-indexed
//! `(line, character)` where `character` is a code-unit count in the
//! negotiated [`SourceEncoding`]:
//!
//! - [`SourceEncoding::UTF8`]: `character` is a UTF-8 byte count
//!   within the line.
//! - [`SourceEncoding::UTF16`]: `character` is a UTF-16 code-unit
//!   count within the line (the LSP default).
//!
//! Every consumer that translates between LSP positions and internal
//! byte offsets goes through this module, so the encoding contract
//! stays in lockstep with what the client negotiated.

use std::ops::Range;

use lsp_types::{self, Position};

use crate::SourceEncoding;

#[inline]
fn unit_len(c: char, encoding: SourceEncoding) -> u32 {
    match encoding {
        SourceEncoding::UTF8 => c.len_utf8() as u32,
        SourceEncoding::UTF16 => c.len_utf16() as u32,
    }
}

pub fn position_to_byte(text: &str, pos: Position, encoding: SourceEncoding) -> usize {
    let mut line = 0u32;
    let mut byte = 0usize;
    for c in text.chars() {
        if line == pos.line {
            break;
        }
        byte += c.len_utf8();
        if c == '\n' {
            line += 1;
        }
    }
    let mut col = 0u32;
    let bytes = text.as_bytes();
    while col < pos.character && byte < bytes.len() {
        if bytes[byte] == b'\n' {
            break;
        }
        let c = text[byte..].chars().next().unwrap();
        byte += c.len_utf8();
        col += unit_len(c, encoding);
    }
    byte
}

pub fn byte_to_position(text: &str, byte: usize, encoding: SourceEncoding) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    let prefix = &text[..byte.min(text.len())];
    for c in prefix.chars() {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += unit_len(c, encoding);
        }
    }
    Position {
        line,
        character: col,
    }
}

pub fn byte_range_to_lsp(
    text: &str,
    range: &Range<usize>,
    encoding: SourceEncoding,
) -> lsp_types::Range {
    lsp_types::Range {
        start: byte_to_position(text, range.start, encoding),
        end: byte_to_position(text, range.end, encoding),
    }
}

pub fn ranges_overlap(a: &lsp_types::Range, b: &lsp_types::Range) -> bool {
    !(a.end.line < b.start.line
        || a.start.line > b.end.line
        || (a.end.line == b.start.line && a.end.character < b.start.character)
        || (a.start.line == b.end.line && a.start.character > b.end.character))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_to_position_utf8_counts_bytes() {
        // `—` (em-dash) is 3 bytes UTF-8, 1 UTF-16 unit.
        let text = "ab — cd\n";
        // After `ab ` (3 bytes), the em-dash spans bytes 3..6.
        let pos = byte_to_position(text, 6, SourceEncoding::UTF8);
        assert_eq!(
            pos,
            Position {
                line: 0,
                character: 6
            }
        );
    }

    #[test]
    fn byte_to_position_utf16_counts_code_units() {
        let text = "ab — cd\n";
        // 6 bytes in == `ab —` == 4 UTF-16 units (`a`, `b`, ` `, `—`).
        let pos = byte_to_position(text, 6, SourceEncoding::UTF16);
        assert_eq!(
            pos,
            Position {
                line: 0,
                character: 4
            }
        );
    }

    #[test]
    fn position_to_byte_utf16_round_trip_past_multibyte() {
        let text = "hello — world\n";
        // `hello ` (6 units) + `—` (1 unit) = 7 units; bytes = 6 + 3 = 9.
        let byte = position_to_byte(
            text,
            Position {
                line: 0,
                character: 7,
            },
            SourceEncoding::UTF16,
        );
        assert_eq!(byte, 9);
    }

    #[test]
    fn byte_range_to_lsp_utf16_clamps_doc_comment_line() {
        // doc_comment line that spans an em-dash — under UTF-16 the
        // line is 17 code units; under UTF-8 it's 19 bytes.
        let text = "/// hello — world\nfn main() {}\n";
        let line_end = text.find('\n').unwrap(); // 19 bytes
        let lsp_range = byte_range_to_lsp(text, &(0..line_end), SourceEncoding::UTF16);
        assert_eq!(lsp_range.end.character, 17);
        let lsp_range = byte_range_to_lsp(text, &(0..line_end), SourceEncoding::UTF8);
        assert_eq!(lsp_range.end.character, 19);
    }
}
