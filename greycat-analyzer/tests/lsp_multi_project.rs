//! P32.2 — eager multi-project workspace discovery.
//!
//! Spawns `greycat-analyzer server` with two sibling `project.gcl`
//! roots in one workspace and asserts the server eagerly loads
//! both projects and publishes diagnostics for each entrypoint.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn workspace_with_two_sibling_projects_loads_both() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lsp_multi_project");
    let _ = std::fs::remove_dir_all(&base);
    let proj_a = base.join("projA");
    let proj_b = base.join("projB");
    std::fs::create_dir_all(&proj_a).expect("mkdir projA");
    std::fs::create_dir_all(&proj_b).expect("mkdir projB");

    let proj_a_gcl = proj_a.join("project.gcl");
    let proj_b_gcl = proj_b.join("project.gcl");
    // Minimal valid module per project. No stdlib pull — keeps the
    // test hermetic (no `greycat install` required) and load time
    // tiny.
    std::fs::write(&proj_a_gcl, "fn a(): int { return 1; }\n").unwrap();
    std::fs::write(&proj_b_gcl, "fn b(): int { return 2; }\n").unwrap();

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

    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [
                { "uri": uri_for(&proj_a), "name": "projA" },
                { "uri": uri_for(&proj_b), "name": "projB" },
            ],
            "capabilities": {},
        }
    });
    write_msg(&mut stdin, &init_req);
    let resp = read_msg(&mut stdout, Duration::from_secs(5));
    assert_eq!(resp["id"], 1, "initialize response id mismatch: {resp}");

    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );

    // Both project.gcl entrypoints must publish diagnostics during
    // the eager workspace load. Order is unspecified (HashMap iteration).
    let entry_a = uri_for(&proj_a_gcl);
    let entry_b = uri_for(&proj_b_gcl);
    let mut seen_a = false;
    let mut seen_b = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !(seen_a && seen_b) && Instant::now() < deadline {
        let msg = read_msg(&mut stdout, Duration::from_secs(10));
        if msg["method"] == "textDocument/publishDiagnostics" {
            let uri = msg["params"]["uri"].as_str().unwrap_or("");
            if uri == entry_a {
                seen_a = true;
            }
            if uri == entry_b {
                seen_b = true;
            }
        }
    }

    // Tidy shutdown so the child exits cleanly.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null
        }),
    );
    let _ = read_until(&mut stdout, |m| m["id"] == 99, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "method": "exit", "params": null
        }),
    );
    drop(stdin);
    let _ = child.wait();

    assert!(
        seen_a,
        "no publishDiagnostics for projA entrypoint ({entry_a})"
    );
    assert!(
        seen_b,
        "no publishDiagnostics for projB entrypoint ({entry_b})"
    );
}

fn uri_for(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn write_msg(w: &mut impl Write, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(w, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
}

fn read_msg(r: &mut BufReader<impl Read>, timeout: Duration) -> serde_json::Value {
    let started = Instant::now();
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
