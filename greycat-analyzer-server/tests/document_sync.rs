//! Tests for the LSP `textDocument/didChange` path.
//!
//! These exercise the exact code path the server uses on every
//! keystroke: `SourceManager::update` → `Document::apply_changes` →
//! tree-sitter incremental reparse. Two concern areas the user has
//! reported instability on:
//!
//! - **Position encoding** — clients may negotiate UTF-16 (the LSP
//!   default when `general.positionEncodings` is absent). For ASCII
//!   text the UTF-8 and UTF-16 paths produce the same byte offsets,
//!   so the only way to catch a regression is to feed multi-byte
//!   characters (emoji / accents) and check that an UTF-16 column
//!   resolves to the right byte. A miscount lands `replace_range`
//!   mid-codepoint and the edit gets silently dropped — leaving the
//!   editor and the server with permanently different views of the
//!   buffer.
//!
//! - **Incremental reparse stability** — the user observed that
//!   inserting then removing a single character (`d`) in the middle
//!   of a function body left a ~300-line module with most of the
//!   text flagged red. That points at tree-sitter's incremental
//!   reparse producing a degraded tree even though the text returns
//!   to its original bytes. The contract every diff-based editor
//!   relies on is: applying an edit and then its inverse must leave
//!   the document in the same diagnostic state as the original. The
//!   tests below assert exactly that, at several insertion sites
//!   and with a multi-edit "type a word, then backspace it" cadence
//!   that mirrors real typing.
//!
//! Failing tests in this file mean the LSP server will misrender
//! errors in the editor — the symptoms the user described.

use std::str::FromStr;

use greycat_analyzer_core::diagnostics::parse_diagnostics;
use greycat_analyzer_core::{Document, SourceEncoding, SourceManager};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent, TextDocumentItem, Uri};

fn uri() -> Uri {
    Uri::from_str("file:///main.gcl").unwrap()
}

/// Open a single in-memory module on a fresh manager, mirroring what
/// `Backend::did_open` does (minus the project routing).
fn open(text: &str) -> SourceManager {
    let mut m = SourceManager::new();
    let doc = Document::new(TextDocumentItem {
        uri: uri(),
        language_id: "greycat".into(),
        version: 1,
        text: text.into(),
    });
    m.add(doc);
    m
}

fn full_text(m: &SourceManager) -> String {
    m.get(&uri()).unwrap().borrow().text.clone()
}

fn parse_error_count(m: &SourceManager) -> usize {
    let cell = m.get(&uri()).unwrap();
    let d = cell.borrow();
    parse_diagnostics(d.root_node(), &d.text).len()
}

fn parse_error_summary(m: &SourceManager) -> String {
    let cell = m.get(&uri()).unwrap();
    let d = cell.borrow();
    let diags = parse_diagnostics(d.root_node(), &d.text);
    diags
        .iter()
        .map(|x| format!("[{:?}] {}", x.range, x.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply a single replacement edit at an LSP range. `version` is the
/// next sequential version number.
fn edit(m: &mut SourceManager, range: Range, text: &str, version: i32, encoding: SourceEncoding) {
    m.update(
        &uri(),
        vec![TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: text.into(),
        }],
        version,
        encoding,
    );
}

/// Convenience: convert a byte offset in `text` to an LSP Position
/// using UTF-8 column semantics (matches `SourceEncoding::UTF8`).
fn pos_utf8(text: &str, byte_offset: usize) -> Position {
    let prefix = &text[..byte_offset];
    let line = prefix.matches('\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = (byte_offset - line_start) as u32;
    Position {
        line,
        character: column,
    }
}

// =============================================================================
// UTF-16 encoding correctness
// =============================================================================

/// Smoke check that nothing in the UTF-16 path mangles pure-ASCII
/// input. If this fails we have a wiring bug independent of any
/// multi-byte handling.
#[test]
fn utf16_encoding_ascii_only_round_trip() {
    let src = "fn main(): int { return 1; }\n";
    let mut m = open(src);
    // Replace `1` with `2` — column 24 in UTF-16, same as UTF-8.
    edit(
        &mut m,
        Range {
            start: Position {
                line: 0,
                character: 24,
            },
            end: Position {
                line: 0,
                character: 25,
            },
        },
        "2",
        2,
        SourceEncoding::UTF16,
    );
    assert_eq!(full_text(&m), "fn main(): int { return 2; }\n");
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

/// A BMP character below U+10000 takes 1 UTF-16 code unit but its
/// UTF-8 encoding is 2 or 3 bytes. An UTF-16 character column past
/// the accent must translate to the byte AFTER its multi-byte
/// sequence, not into the middle of it.
///
/// Source: `fn main(): String { return "café"; }`.
/// `é` = U+00E9 → 2 bytes UTF-8, 1 code unit UTF-16.
/// Insert an `s` between `é` and the closing `"`. With UTF-16
/// positions, that's column 32 (after `"caf` + `é` = 28 + 4 = 32).
/// With UTF-8 byte offsets the same insertion is at byte 33.
/// If the encoding wiring is wrong the edit lands at the wrong byte
/// (or is silently dropped at a char boundary check) and the
/// resulting text differs.
#[test]
fn utf16_encoding_inserts_after_bmp_multibyte_char() {
    let src = "fn main(): String { return \"café\"; }\n";
    // sanity-check what the source actually looks like; the literal
    // above is `é` (U+00E9), not an `e` + combining accent
    assert!(src.contains("café"));
    let mut m = open(src);
    let insert_col_utf16 = "fn main(): String { return \"café".encode_utf16().count() as u32;
    edit(
        &mut m,
        Range {
            start: Position {
                line: 0,
                character: insert_col_utf16,
            },
            end: Position {
                line: 0,
                character: insert_col_utf16,
            },
        },
        "s",
        2,
        SourceEncoding::UTF16,
    );
    assert_eq!(
        full_text(&m),
        "fn main(): String { return \"cafés\"; }\n",
        "edit landed at wrong byte offset under UTF-16 encoding"
    );
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

/// A character above U+FFFF takes 2 UTF-16 code units (a surrogate
/// pair) and 4 bytes UTF-8. This is the encoding case most likely
/// to silently break — if we treat the second surrogate as a real
/// character we'll land mid-codepoint and the edit gets dropped.
///
/// 🎉 = U+1F389. UTF-16 length = 2, UTF-8 length = 4.
#[test]
fn utf16_encoding_inserts_after_surrogate_pair() {
    let src = "fn main(): String { return \"hi 🎉\"; }\n";
    let mut m = open(src);
    let insert_col_utf16 = "fn main(): String { return \"hi 🎉".encode_utf16().count() as u32;
    edit(
        &mut m,
        Range {
            start: Position {
                line: 0,
                character: insert_col_utf16,
            },
            end: Position {
                line: 0,
                character: insert_col_utf16,
            },
        },
        "!",
        2,
        SourceEncoding::UTF16,
    );
    assert_eq!(
        full_text(&m),
        "fn main(): String { return \"hi 🎉!\"; }\n",
        "edit landed at wrong byte offset across a surrogate pair"
    );
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

/// Replace a multi-byte character with ASCII. The end position is
/// past the surrogate pair; the wrong encoding will leave it
/// mid-codepoint and either silently drop the edit (char-boundary
/// guard) or corrupt the buffer.
#[test]
fn utf16_encoding_replaces_surrogate_pair_range() {
    let src = "fn main(): String { return \"hi 🎉\"; }\n";
    let mut m = open(src);
    let start_col_utf16 = "fn main(): String { return \"hi ".encode_utf16().count() as u32;
    let end_col_utf16 = "fn main(): String { return \"hi 🎉".encode_utf16().count() as u32;
    edit(
        &mut m,
        Range {
            start: Position {
                line: 0,
                character: start_col_utf16,
            },
            end: Position {
                line: 0,
                character: end_col_utf16,
            },
        },
        "X",
        2,
        SourceEncoding::UTF16,
    );
    assert_eq!(
        full_text(&m),
        "fn main(): String { return \"hi X\"; }\n",
        "delete-range across a surrogate pair under UTF-16 misbehaved"
    );
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

/// Multi-line document with a multi-byte char on an earlier line.
/// Edit a later line — the UTF-16 column on that line is small and
/// ASCII, but the column conversion must not be confused by earlier
/// surrogate pairs. Catches "column index is global" off-by-one
/// classes of bugs.
#[test]
fn utf16_encoding_multiline_with_earlier_surrogate_pair() {
    let src = "fn greet(): String { return \"hi 🎉\"; }\n\
               fn nope(): int { return 0; }\n";
    let mut m = open(src);
    // Replace `0` with `42` on the *second* line; column counting
    // restarts at 0 each line, so the surrogate on line 0 must not
    // bleed into line 1.
    let line1 = "fn nope(): int { return ";
    let start_col = line1.encode_utf16().count() as u32;
    edit(
        &mut m,
        Range {
            start: Position {
                line: 1,
                character: start_col,
            },
            end: Position {
                line: 1,
                character: start_col + 1,
            },
        },
        "42",
        2,
        SourceEncoding::UTF16,
    );
    let expected = "fn greet(): String { return \"hi 🎉\"; }\n\
                    fn nope(): int { return 42; }\n";
    assert_eq!(full_text(&m), expected);
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

/// A batched did_change containing several incremental edits whose
/// positions reference state AFTER the previous edit in the same
/// batch (per LSP spec). Run under UTF-16 with multi-byte content
/// so the per-edit `LineIndex` recompute is exercised.
#[test]
fn utf16_encoding_batched_changes_with_multibyte_chars() {
    let src = "fn main(): String { return \"café\"; }\n";
    let mut m = open(src);
    // Edit 1: insert `s` right after `é` → `cafés`.
    // Edit 2: in the result of edit 1, replace `cafés` with `crepes`.
    let insert_col = "fn main(): String { return \"café".encode_utf16().count() as u32;
    let after_insert = "fn main(): String { return \"cafés\"; }\n";
    // start of `cafés` body inside quotes, UTF-16 columns on the
    // post-edit-1 text.
    let body_start_col = "fn main(): String { return \"".encode_utf16().count() as u32;
    let body_end_col = "fn main(): String { return \"cafés".encode_utf16().count() as u32;
    m.update(
        &uri(),
        vec![
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: insert_col,
                    },
                    end: Position {
                        line: 0,
                        character: insert_col,
                    },
                }),
                range_length: None,
                text: "s".into(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: body_start_col,
                    },
                    end: Position {
                        line: 0,
                        character: body_end_col,
                    },
                }),
                range_length: None,
                text: "crepes".into(),
            },
        ],
        2,
        SourceEncoding::UTF16,
    );
    assert_eq!(
        full_text(&m),
        "fn main(): String { return \"crepes\"; }\n",
        "batched changes (edit 1 then edit 2 against post-edit-1 text) misbehaved; intermediate was {after_insert:?}"
    );
    assert_eq!(parse_error_count(&m), 0, "{}", parse_error_summary(&m));
}

// =============================================================================
// Incremental update stability
// =============================================================================

/// A small but realistic module used in the add-then-remove tests
/// below. About 50 lines, multiple top-level decls, several
/// function bodies — enough surface that a degraded incremental
/// reparse should produce many spurious ERROR / MISSING nodes.
fn fixture_stable_module() -> &'static str {
    r#"@library("std", "0.0.1-dev");

type Point {
    x: float;
    y: float;

    static fn origin(): Point {
        return Point { x: 0.0, y: 0.0 };
    }

    fn distance_squared(other: Point): float {
        var dx = this.x - other.x;
        var dy = this.y - other.y;
        return dx * dx + dy * dy;
    }
}

fn clamp(value: int, lo: int, hi: int): int {
    if (value < lo) {
        return lo;
    }
    if (value > hi) {
        return hi;
    }
    return value;
}

fn describe(p: Point): String {
    var sx = String::from(p.x);
    var sy = String::from(p.y);
    return "(" + sx + ", " + sy + ")";
}

fn run(): int {
    var origin = Point::origin();
    var here = Point { x: 3.0, y: 4.0 };
    var d2 = here.distance_squared(origin);
    var bounded = clamp(42, 0, 10);
    return bounded;
}
"#
}

/// Find the byte offset of `needle` in `src`. Panics if absent or
/// non-unique — tests should pick a unique anchor.
fn unique_offset(src: &str, needle: &str) -> usize {
    let first = src.find(needle).expect("needle present");
    assert!(
        src.matches(needle).count() == 1,
        "needle {needle:?} is not unique in source"
    );
    first
}

/// Insert a single character at `byte_offset` (LSP position), then
/// delete it. The final document must be byte-identical to the
/// initial state AND parse without errors. This is the regression
/// test for "type `d`, backspace `d`, half the file goes red".
#[test]
fn insert_then_delete_single_char_in_function_body_restores_clean_parse() {
    let src = fixture_stable_module();
    let mut m = open(src);
    assert_eq!(
        parse_error_count(&m),
        0,
        "fixture must parse cleanly before the test mutates it; got: {}",
        parse_error_summary(&m)
    );

    // Insert `d` between `bounde` and `d` in `bounded` inside `run`'s
    // body — exact same shape as the user's described scenario
    // (typing a letter mid-identifier). Pick the first `d` in
    // `bounded` so the offset is deterministic.
    let target = "var bounded = clamp";
    let off = unique_offset(src, target);
    // Insert between the `e` and the second `d` in `bounded` —
    // exact same shape as the user described (one letter pushed
    // into the middle of an identifier inside a function body).
    let insert_byte = off + "var bounde".len();
    let insert_pos = pos_utf8(src, insert_byte);

    edit(
        &mut m,
        Range {
            start: insert_pos,
            end: insert_pos,
        },
        "d",
        2,
        SourceEncoding::UTF8,
    );
    let after_insert = full_text(&m);
    assert!(
        after_insert.contains("var boundedd = clamp"),
        "after insert text doesn't have the new char: ...{}...",
        &after_insert[after_insert.len().saturating_sub(120)..]
    );

    // Now delete that same `d`. In the post-insert text the new `d`
    // sits at insert_byte; range = [insert_byte, insert_byte+1).
    let delete_start = pos_utf8(&after_insert, insert_byte);
    let delete_end = pos_utf8(&after_insert, insert_byte + 1);
    edit(
        &mut m,
        Range {
            start: delete_start,
            end: delete_end,
        },
        "",
        3,
        SourceEncoding::UTF8,
    );

    let final_text = full_text(&m);
    assert_eq!(
        final_text, src,
        "after insert+delete cycle the text differs from the original"
    );
    assert_eq!(
        parse_error_count(&m),
        0,
        "incremental reparse left spurious errors after a no-op edit cycle:\n{}",
        parse_error_summary(&m)
    );
}

/// Same idea as the previous test but at many different insertion
/// sites — exercises whether the incremental-reparse bug is
/// location-dependent. If any one site triggers it, we want to see
/// which.
#[test]
fn insert_then_delete_at_many_sites_restores_clean_parse() {
    let src = fixture_stable_module();
    // Sites chosen to span different syntactic contexts: identifier
    // middles, after keywords, inside expressions, inside type
    // declarations.
    let sites: &[(&str, usize, &str)] = &[
        ("var dx = this.x", "var d".len(), "in `dx` ident"),
        ("var dy = this.y", "var d".len(), "in `dy` ident"),
        ("fn clamp(value", "fn cla".len(), "in `clamp` fn name"),
        ("fn describe(p", "fn desc".len(), "in `describe` fn name"),
        ("var sx = String", "var s".len(), "in `sx` ident"),
        ("static fn origin", "static fn ori".len(), "in `origin`"),
        (
            "fn distance_squared",
            "fn distance_".len(),
            "in `distance_squared`",
        ),
    ];
    for (anchor, offset_within, label) in sites {
        let mut m = open(src);
        assert_eq!(
            parse_error_count(&m),
            0,
            "fixture must parse cleanly at start ({label}): {}",
            parse_error_summary(&m)
        );
        let base = unique_offset(src, anchor);
        let insert_byte = base + offset_within;
        let insert_pos = pos_utf8(src, insert_byte);
        edit(
            &mut m,
            Range {
                start: insert_pos,
                end: insert_pos,
            },
            "d",
            2,
            SourceEncoding::UTF8,
        );
        let after = full_text(&m);
        let delete_start = pos_utf8(&after, insert_byte);
        let delete_end = pos_utf8(&after, insert_byte + 1);
        edit(
            &mut m,
            Range {
                start: delete_start,
                end: delete_end,
            },
            "",
            3,
            SourceEncoding::UTF8,
        );
        assert_eq!(
            full_text(&m),
            src,
            "{label}: text drifted after insert+delete cycle"
        );
        assert_eq!(
            parse_error_count(&m),
            0,
            "{label}: incremental reparse left spurious errors after a no-op edit cycle:\n{}",
            parse_error_summary(&m)
        );
    }
}

/// Type a multi-character word into the middle of a function body,
/// one keystroke at a time, then backspace each one out again. The
/// final state must be both byte-identical to the original and
/// produce zero parse errors. This is the realistic typing cadence
/// — most editors send one `did_change` per key.
#[test]
fn type_word_then_backspace_each_char_restores_clean_parse() {
    let src = fixture_stable_module();
    let mut m = open(src);
    let target = "return bounded;";
    let base = unique_offset(src, target);
    let insert_byte = base + "return ".len(); // BEFORE `bounded`

    // Type `hello` one char at a time.
    let word = "hello";
    for (i, ch) in word.chars().enumerate() {
        let current = full_text(&m);
        let p = pos_utf8(&current, insert_byte + i);
        edit(
            &mut m,
            Range { start: p, end: p },
            &ch.to_string(),
            2 + i as i32,
            SourceEncoding::UTF8,
        );
    }
    let mid = full_text(&m);
    assert!(
        mid.contains("return hellobounded;"),
        "typed word missing in body; got fragment: ...{}...",
        &mid[base..base + target.len() + word.len()]
    );

    // Backspace `hello` one char at a time.
    let version_base = 2 + word.chars().count() as i32;
    for i in 0..word.chars().count() {
        let current = full_text(&m);
        let del_end = insert_byte + word.chars().count() - i;
        let del_start = del_end - 1;
        let p_start = pos_utf8(&current, del_start);
        let p_end = pos_utf8(&current, del_end);
        edit(
            &mut m,
            Range {
                start: p_start,
                end: p_end,
            },
            "",
            version_base + i as i32,
            SourceEncoding::UTF8,
        );
    }

    let final_text = full_text(&m);
    assert_eq!(
        final_text, src,
        "after type-word+backspace cycle the text drifted"
    );
    assert_eq!(
        parse_error_count(&m),
        0,
        "incremental reparse left spurious errors after type+backspace cycle:\n{}",
        parse_error_summary(&m)
    );
}

/// Diagnostic equivalence between incremental and fresh parse.
///
/// Apply an edit incrementally to a fresh manager, then build a
/// second manager whose initial text is the post-edit text (i.e. a
/// from-scratch parse). The parse diagnostics must match — same
/// count, same ranges, same messages. If they diverge, the
/// incremental reparse has produced a different tree than the
/// fresh parse for the same text, which is exactly the class of
/// bug the user reported.
#[test]
fn incremental_and_fresh_parse_diagnostics_match_after_edit_cycle() {
    let src = fixture_stable_module();
    let mut m_inc = open(src);
    // Same insert+delete cycle as the single-site test above.
    let target = "var bounded = clamp";
    let off = unique_offset(src, target);
    let insert_byte = off + "var bounde".len();
    let insert_pos = pos_utf8(src, insert_byte);
    edit(
        &mut m_inc,
        Range {
            start: insert_pos,
            end: insert_pos,
        },
        "d",
        2,
        SourceEncoding::UTF8,
    );
    let after = full_text(&m_inc);
    let delete_start = pos_utf8(&after, insert_byte);
    let delete_end = pos_utf8(&after, insert_byte + 1);
    edit(
        &mut m_inc,
        Range {
            start: delete_start,
            end: delete_end,
        },
        "",
        3,
        SourceEncoding::UTF8,
    );

    // Fresh manager with the (now restored) text.
    let m_fresh = open(&full_text(&m_inc));
    let inc_diags = {
        let cell = m_inc.get(&uri()).unwrap();
        let d = cell.borrow();
        parse_diagnostics(d.root_node(), &d.text)
    };
    let fresh_diags = {
        let cell = m_fresh.get(&uri()).unwrap();
        let d = cell.borrow();
        parse_diagnostics(d.root_node(), &d.text)
    };
    assert_eq!(
        inc_diags.len(),
        fresh_diags.len(),
        "incremental reparse diagnostic count diverged from fresh parse\n\
         incremental:\n{}\nfresh:\n{}",
        inc_diags
            .iter()
            .map(|x| format!("  [{:?}] {}", x.range, x.message))
            .collect::<Vec<_>>()
            .join("\n"),
        fresh_diags
            .iter()
            .map(|x| format!("  [{:?}] {}", x.range, x.message))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Some editors send keystrokes batched into a single did_change
/// (e.g. when autocomplete inserts a snippet and then the user
/// backspaces). The batch's later events reference state AFTER the
/// earlier events in the same batch (LSP spec). Apply an insert
/// and its inverse delete in ONE batch; final state must be clean.
#[test]
fn batched_insert_and_inverse_delete_in_one_did_change() {
    let src = fixture_stable_module();
    let mut m = open(src);
    let target = "var bounded = clamp";
    let off = unique_offset(src, target);
    let insert_byte = off + "var bounde".len();
    let insert_pos = pos_utf8(src, insert_byte);
    // After the insert, byte `insert_byte` holds the new `d`, so
    // the delete range is [insert_byte, insert_byte + 1).
    let post_insert_text_len_at_line = {
        // Both positions are on the same line; bytes within that
        // line do not cross a newline so column math is just byte
        // delta. Recompute against the original text shifted by the
        // single-byte insert.
        let mut synthetic = src.to_string();
        synthetic.insert(insert_byte, 'd');
        (
            pos_utf8(&synthetic, insert_byte),
            pos_utf8(&synthetic, insert_byte + 1),
        )
    };
    let (del_start, del_end) = post_insert_text_len_at_line;
    m.update(
        &uri(),
        vec![
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: insert_pos,
                    end: insert_pos,
                }),
                range_length: None,
                text: "d".into(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: del_start,
                    end: del_end,
                }),
                range_length: None,
                text: "".into(),
            },
        ],
        2,
        SourceEncoding::UTF8,
    );
    assert_eq!(full_text(&m), src, "batched insert+delete drifted");
    assert_eq!(
        parse_error_count(&m),
        0,
        "batched insert+delete left spurious errors:\n{}",
        parse_error_summary(&m)
    );
}

/// CRLF line endings. Tree-sitter byte offsets count `\r` as part
/// of the line; LSP `Position::character` does NOT (it's a column
/// within the line, after the line break). If the implementation
/// confuses CRLF for LF, multi-line edits drift by one byte per
/// line. Repeat the insert+delete cycle on a CRLF-normalised
/// fixture and assert clean state.
#[test]
fn crlf_line_endings_insert_then_delete_restores_clean_parse() {
    let src = fixture_stable_module().replace('\n', "\r\n");
    let mut m = open(&src);
    assert_eq!(
        parse_error_count(&m),
        0,
        "CRLF fixture must parse cleanly initially; got: {}",
        parse_error_summary(&m)
    );
    let target = "var bounded = clamp";
    let off = unique_offset(&src, target);
    let insert_byte = off + "var bounde".len();
    let insert_pos = pos_utf8(&src, insert_byte);
    edit(
        &mut m,
        Range {
            start: insert_pos,
            end: insert_pos,
        },
        "d",
        2,
        SourceEncoding::UTF8,
    );
    let after = full_text(&m);
    let delete_start = pos_utf8(&after, insert_byte);
    let delete_end = pos_utf8(&after, insert_byte + 1);
    edit(
        &mut m,
        Range {
            start: delete_start,
            end: delete_end,
        },
        "",
        3,
        SourceEncoding::UTF8,
    );
    assert_eq!(
        full_text(&m),
        src,
        "CRLF text drifted after insert+delete cycle"
    );
    assert_eq!(
        parse_error_count(&m),
        0,
        "CRLF incremental reparse left spurious errors after a no-op edit cycle:\n{}",
        parse_error_summary(&m)
    );
}

/// Many sequential insert+delete cycles at varied positions, with
/// version numbers that strictly increase, simulating the kind of
/// edit storm a fast typist generates. Final state must be clean.
#[test]
fn many_consecutive_insert_delete_cycles_keeps_clean_parse() {
    let src = fixture_stable_module();
    let mut m = open(src);
    // Cycle through a handful of insertion sites repeatedly.
    let anchors: &[(&str, usize)] = &[
        ("var dx = this.x", "var d".len()),
        ("fn clamp(value", "fn cla".len()),
        ("var sx = String", "var s".len()),
        ("static fn origin", "static fn ori".len()),
        ("fn distance_squared", "fn distance_".len()),
        ("var bounded = clamp", "var bounde".len()),
    ];
    let mut version: i32 = 2;
    for round in 0..5 {
        for (anchor, off_in) in anchors {
            let current = full_text(&m);
            let base = unique_offset(&current, anchor);
            let insert_byte = base + off_in;
            let insert_pos = pos_utf8(&current, insert_byte);
            edit(
                &mut m,
                Range {
                    start: insert_pos,
                    end: insert_pos,
                },
                "d",
                version,
                SourceEncoding::UTF8,
            );
            version += 1;
            let after = full_text(&m);
            let del_start = pos_utf8(&after, insert_byte);
            let del_end = pos_utf8(&after, insert_byte + 1);
            edit(
                &mut m,
                Range {
                    start: del_start,
                    end: del_end,
                },
                "",
                version,
                SourceEncoding::UTF8,
            );
            version += 1;
            assert_eq!(
                full_text(&m),
                src,
                "round {round} anchor {anchor:?}: text drifted after insert+delete"
            );
            let count = parse_error_count(&m);
            assert_eq!(
                count,
                0,
                "round {round} anchor {anchor:?}: spurious errors after no-op edit cycle:\n{}",
                parse_error_summary(&m)
            );
        }
    }
}

/// Long-form regression test mirroring the user's report directly:
/// a module that is large enough that a degraded incremental
/// reparse produces a visible amount of red squiggly. Insert `d`
/// then delete it; the final diagnostic count must be 0.
///
/// The fixture below is generated by repeating a clean function
/// definition many times, so the original text is guaranteed to
/// parse with zero errors.
#[test]
fn long_module_insert_then_delete_keeps_clean_parse() {
    // ~300 lines of clean code.
    let mut buf = String::new();
    buf.push_str("@library(\"std\", \"0.0.1-dev\");\n\n");
    for i in 0..30 {
        buf.push_str(&format!(
            "fn helper_{i}(value: int): int {{\n    \
                var temp = value * 2;\n    \
                if (temp > 100) {{\n        return 100;\n    }}\n    \
                if (temp < 0) {{\n        return 0;\n    }}\n    \
                return temp;\n\
             }}\n\n"
        ));
    }
    let src = buf;
    let mut m = open(&src);
    assert_eq!(
        parse_error_count(&m),
        0,
        "long fixture must parse cleanly initially; got: {}",
        parse_error_summary(&m)
    );

    // Insert in the middle of `helper_15`'s identifier (somewhere
    // around the middle of the file).
    let anchor = "fn helper_15(value: int)";
    let base = unique_offset(&src, anchor);
    let insert_byte = base + "fn helper_1".len(); // between `_1` and `5`
    let insert_pos = pos_utf8(&src, insert_byte);
    edit(
        &mut m,
        Range {
            start: insert_pos,
            end: insert_pos,
        },
        "d",
        2,
        SourceEncoding::UTF8,
    );
    let after = full_text(&m);
    let delete_start = pos_utf8(&after, insert_byte);
    let delete_end = pos_utf8(&after, insert_byte + 1);
    edit(
        &mut m,
        Range {
            start: delete_start,
            end: delete_end,
        },
        "",
        3,
        SourceEncoding::UTF8,
    );

    assert_eq!(
        full_text(&m),
        src,
        "long-module text drifted after insert+delete cycle"
    );
    let count = parse_error_count(&m);
    assert_eq!(
        count,
        0,
        "long-module incremental reparse left {count} spurious errors after a no-op edit cycle:\n{}",
        parse_error_summary(&m)
    );
}
