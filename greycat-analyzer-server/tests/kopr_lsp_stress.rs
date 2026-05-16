//! LSP stability stress harness driven by a real on-disk module.
//!
//! Skipped by default — set `GCL_STRESS_FILE=/abs/path/to/file.gcl`
//! to run. The point is to exercise incremental reparse + analyzer
//! invalidation against a module with the same syntactic shape the
//! user reported the parse-degradation bug on (long `@expose` types,
//! deep generic instantiations, string interpolation, etc.).
//!
//! For each anchor we find in the file, the harness inserts a `d`,
//! records the diagnostic count, undoes the insert, and asserts the
//! count returns to the initial. The synthetic tests in
//! `document_sync.rs` cover the same invariant on hand-built
//! fixtures; this one runs the same shape on whatever real source
//! the operator points at.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use greycat_analyzer_core::diagnostics::parse_diagnostics;
use greycat_analyzer_core::{Document, SourceEncoding, SourceManager};
use lsp_types::{Position, Range, TextDocumentContentChangeEvent, TextDocumentItem, Uri};

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

fn parse_error_count(m: &SourceManager, uri: &Uri) -> usize {
    let cell = m.get(uri).unwrap();
    let d = cell.borrow();
    parse_diagnostics(d.root_node(), &d.text).len()
}

/// Sample evenly-spaced byte offsets across the file, snapping each
/// to the nearest char boundary that sits inside an identifier-ish
/// region (alnum / underscore) so the inserted `d` lands inside an
/// existing word rather than breaking a token boundary. Skips lines
/// that look like comments — those are tree-sitter's easy mode.
fn pick_anchors(text: &str) -> Vec<(String, usize)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n < 200 {
        return Vec::new();
    }
    let mut out: Vec<(String, usize)> = Vec::new();
    let step = (n / 64).max(64);
    let mut off = step;
    while off + 1 < n {
        // Walk to the next ASCII identifier middle (a letter that is
        // followed and preceded by another identifier byte).
        let mut probe = off;
        while probe + 1 < n {
            let b = bytes[probe];
            let bp = if probe == 0 { b' ' } else { bytes[probe - 1] };
            let bn = bytes[probe + 1];
            let id_byte = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
            if id_byte(b) && id_byte(bp) && id_byte(bn) && text.is_char_boundary(probe) {
                break;
            }
            probe += 1;
        }
        if probe + 1 >= n {
            break;
        }
        // Skip if the line starts with `//` (a comment edit is too easy).
        let line_start = text[..probe].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_prefix = &text[line_start..probe];
        if line_prefix.trim_start().starts_with("//") {
            off = probe + step;
            continue;
        }
        out.push((format!("byte {probe}"), probe));
        off = probe + step;
    }
    out
}

#[test]
#[ignore]
fn stress_real_file_insert_delete_cycle() {
    let path: PathBuf = match env::var("GCL_STRESS_FILE") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("skip: set GCL_STRESS_FILE=/abs/path/to/file.gcl");
            return;
        }
    };
    let text = fs::read_to_string(&path).expect("read source");
    let uri = Uri::from_str("file:///stress.gcl").unwrap();
    let mut mgr = SourceManager::new();
    mgr.add(Document::new(TextDocumentItem {
        uri: uri.clone(),
        language_id: "greycat".into(),
        version: 1,
        text: text.clone(),
    }));
    let initial = parse_error_count(&mgr, &uri);
    eprintln!(
        "[stress] {} :: initial parse errors = {initial}",
        path.display()
    );

    let anchors = pick_anchors(&text);
    eprintln!("[stress] picked {} anchors", anchors.len());

    let mut failures: Vec<(String, usize, usize)> = Vec::new();
    let mut version: i32 = 2;
    // Type a 5-char word one keystroke at a time, then backspace each
    // — mirrors the real-typing cadence the user reported the bug on.
    let word = "hello";
    for (anchor, insert_byte) in anchors {
        // Insert `word` one char at a time.
        for (i, ch) in word.chars().enumerate() {
            let current = mgr.get(&uri).unwrap().borrow().text.clone();
            let p = pos_utf8(&current, insert_byte + i);
            mgr.update(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range { start: p, end: p }),
                    range_length: None,
                    text: ch.to_string(),
                }],
                version,
                SourceEncoding::UTF8,
            );
            version += 1;
        }
        let mid_count = parse_error_count(&mgr, &uri);
        // Backspace `word` one char at a time.
        for i in 0..word.chars().count() {
            let current = mgr.get(&uri).unwrap().borrow().text.clone();
            let del_end = insert_byte + word.chars().count() - i;
            let del_start = del_end - 1;
            let p_start = pos_utf8(&current, del_start);
            let p_end = pos_utf8(&current, del_end);
            mgr.update(
                &uri,
                vec![TextDocumentContentChangeEvent {
                    range: Some(Range {
                        start: p_start,
                        end: p_end,
                    }),
                    range_length: None,
                    text: "".into(),
                }],
                version,
                SourceEncoding::UTF8,
            );
            version += 1;
        }
        let restored = mgr.get(&uri).unwrap().borrow().text.clone();
        let final_count = parse_error_count(&mgr, &uri);
        if restored != text || final_count != initial {
            failures.push((anchor.clone(), final_count, mid_count));
            eprintln!(
                "[stress] anchor {:?} drift: text_equal={}, final={final_count}, mid={mid_count}",
                anchor,
                restored == text
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} anchor(s) left the parser in a degraded state after the no-op cycle",
        failures.len()
    );
}
