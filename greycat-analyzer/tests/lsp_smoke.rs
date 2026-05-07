//! End-to-end LSP protocol smoke test.
//!
//! Spawns the `greycat-analyzer server` binary as a subprocess and
//! exchanges a small JSON-RPC sequence over stdio:
//!   1. `initialize` — must come back with our advertised capabilities.
//!   2. `initialized` notification.
//!   3. `textDocument/didOpen` for a small fixture.
//!   4. `textDocument/hover` at a known cursor — must come back with a
//!      `markdown` body containing the inferred type for a parameter.
//!   5. `shutdown` + `exit`.
//!
//! That's enough to catch the EPIPE-class regressions the user just
//! reported (binary doesn't recognize its own subcommand, doesn't
//! initialize, doesn't reply to a hover) without porting the full
//! 15-file `lsp.*.test.ts` suite. Per-capability behavior is covered
//! by `greycat-analyzer-ls/tests/capabilities.rs`.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE: &str = "fn id(name: String): String { return name; }\n";

#[test]
fn lsp_protocol_initialize_and_hover() {
    let bin = env!("CARGO_BIN_EXE_greycat-analyzer");
    let mut child = Command::new(bin)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn greycat-analyzer server");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // ---- initialize ----
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
        }
    });
    write_msg(&mut stdin, &init_req);

    let resp = read_msg(&mut stdout, Duration::from_secs(5));
    assert_eq!(resp["id"], 1, "initialize response id mismatch: {resp}");
    let caps = &resp["result"]["capabilities"];
    assert!(
        caps["hoverProvider"].is_boolean() || caps["hoverProvider"].is_object(),
        "missing hoverProvider in capabilities: {caps}"
    );
    assert!(
        caps["definitionProvider"].is_boolean() || caps["definitionProvider"].is_object(),
        "missing definitionProvider"
    );

    // ---- initialized ----
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );

    // ---- didOpen ----
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": "file:///mod.gcl",
                    "languageId": "greycat",
                    "version": 1,
                    "text": FIXTURE,
                }
            }
        }),
    );

    // The server publishes diagnostics on didOpen; drain the stream
    // until we see the publishDiagnostics notification, then continue.
    let _diags = read_until(
        &mut stdout,
        |msg| msg["method"] == "textDocument/publishDiagnostics",
        Duration::from_secs(5),
    );

    // ---- hover at line 0, character 30 (somewhere inside the body's
    // `name` use). The fixture is one line:
    //   `fn id(name: String): String { return name; }`
    //   0123456789012345678901234567890123456789012345
    //                                  ^ col 35 = `n` of the second `name`
    let hover_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": "file:///mod.gcl" },
            "position": { "line": 0, "character": 38 },
        }
    });
    write_msg(&mut stdin, &hover_req);

    let hover_resp = read_until(&mut stdout, |msg| msg["id"] == 2, Duration::from_secs(5));
    let value = hover_resp["result"]["contents"]["value"]
        .as_str()
        .unwrap_or("");
    assert!(
        value.contains("String"),
        "hover should mention String, got: {value}"
    );

    // ---- shutdown / exit ----
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": null
        }),
    );
    let _shutdown_resp = read_until(&mut stdout, |msg| msg["id"] == 3, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }),
    );
    drop(stdin);

    let status = child.wait().expect("server exited");
    assert!(status.success(), "server exited non-zero: {status:?}");
}

fn write_msg(w: &mut impl Write, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(w, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
}

fn read_msg(r: &mut BufReader<impl Read>, timeout: Duration) -> serde_json::Value {
    let started = Instant::now();
    // Read headers.
    let mut content_length: Option<usize> = None;
    loop {
        if started.elapsed() > timeout {
            panic!("timed out reading LSP message header");
        }
        let mut line = String::new();
        r.read_line(&mut line).expect("read header");
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(rest) = line.trim_end().strip_prefix("Content-Length: ") {
            content_length = Some(rest.parse().expect("Content-Length number"));
        }
    }
    let len = content_length.expect("Content-Length");
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).expect("read body");
    serde_json::from_slice(&body).expect("parse body")
}

fn read_until(
    r: &mut BufReader<impl Read>,
    pred: impl Fn(&serde_json::Value) -> bool,
    timeout: Duration,
) -> serde_json::Value {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            panic!("timed out waiting for matching LSP message");
        }
        let msg = read_msg(r, timeout);
        if pred(&msg) {
            return msg;
        }
    }
}
