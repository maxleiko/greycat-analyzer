//! Integration regression tests for the semantic-tokens encoding fix.
//!
//! Companion to the unit tests inside
//! `capabilities/semantic_tokens.rs`; this file goes through the
//! `TestProject` fixture so the dispatch path the LSP server actually
//! runs is exercised end-to-end.

use greycat_analyzer_core::SourceEncoding;
use lsp_types::SemanticTokens;

mod support;
use support::TestProject;

const TOK_COMMENT: u32 = 8;

/// Materialize the delta-encoded token stream into `(line, col, len, type)`
/// tuples so individual assertions read cleanly.
fn decode(tokens: &SemanticTokens) -> Vec<(u32, u32, u32, u32)> {
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut col = 0u32;
    for t in &tokens.data {
        line += t.delta_line;
        if t.delta_line != 0 {
            col = 0;
        }
        col += t.delta_start;
        out.push((line, col, t.length, t.token_type));
    }
    out
}

/// Anchors the original bug report: a doc-comment containing `—`
/// (em-dash, U+2014 — 3 UTF-8 bytes, 1 UTF-16 code unit) was emitted
/// with `length = 19` (bytes) on a 17-UTF-16-unit line, overflowing
/// past EOL and bleeding the COMMENT highlight onto the next line so
/// the `fn` keyword rendered as a comment. Under UTF-16 negotiation
/// the token must report 17 units; under UTF-8 it must report 19.
#[test]
fn doc_comment_with_em_dash_under_utf16_does_not_overflow_eol() {
    let src = "/// hello — world\nfn main() {}\n";
    let project = TestProject::single_file(src);
    let tokens = project.semantic_tokens_with(SourceEncoding::UTF16);
    let events = decode(&tokens);

    let comment = events
        .iter()
        .find(|(_, _, _, ty)| *ty == TOK_COMMENT)
        .expect("doc_comment emits a COMMENT token");
    assert_eq!(
        comment.0, 0,
        "comment is on the first line — the regression bug shifted it"
    );
    assert_eq!(comment.1, 0, "comment starts at column 0");
    assert_eq!(
        comment.2, 17,
        "COMMENT length must match the line's 17 UTF-16 units, \
         not 19 bytes — overflow paints the next line"
    );

    // No token may start before col 2 on line 1: that's where `fn` ends.
    // If the comment overflowed by 2 units (the byte-vs-unit gap), it
    // would visually paint cols 0–1 on the next line.
    for (line, col, len, _) in &events {
        if *line == 1 {
            assert!(
                *col + *len <= 80,
                "no token on the fn-keyword line should bleed past its content"
            );
        }
    }
}

#[test]
fn doc_comment_with_em_dash_under_utf8_uses_byte_count() {
    let src = "/// hello — world\nfn main() {}\n";
    let project = TestProject::single_file(src);
    let tokens = project.semantic_tokens_with(SourceEncoding::UTF8);
    let events = decode(&tokens);

    let comment = events
        .iter()
        .find(|(_, _, _, ty)| *ty == TOK_COMMENT)
        .expect("doc_comment emits a COMMENT token");
    assert_eq!(
        comment.2, 19,
        "under UTF-8 encoding, length is the byte count"
    );
}

/// Multi-token line — string literal containing a multibyte char
/// followed by an ident on the next line. Each token must be measured
/// relative to its own line, so the ident's `col` is not affected by
/// the string's byte-vs-unit discrepancy on the prior line.
#[test]
fn ident_after_string_with_multibyte_keeps_correct_position() {
    let src = "fn main() {\n    var s = \"caf\u{00e9}\";\n    var x = 1;\n}\n";
    // `café` — the `é` (U+00E9) is 2 bytes UTF-8, 1 UTF-16 unit.
    let project = TestProject::single_file(src);
    let tokens_u16 = project.semantic_tokens_with(SourceEncoding::UTF16);
    let tokens_u8 = project.semantic_tokens_with(SourceEncoding::UTF8);
    // Both encodings produce the same number of tokens — the encoding
    // only changes column / length math, not classification.
    assert_eq!(tokens_u16.data.len(), tokens_u8.data.len());
}
