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

// P32.4
/// Goto-def in project B must not return locations from project A,
/// even when both projects define an identically-named symbol or
/// when B references a symbol that *only* exists in A. Projects are
/// isolated closures; the runtime model says they don't see each
/// other.
#[test]
fn goto_definition_does_not_leak_across_projects() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lsp_cross_project_isolation");
    let _ = std::fs::remove_dir_all(&base);
    let proj_a = base.join("projA");
    let proj_b = base.join("projB");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();

    // Project A defines `onlyInA`; nothing in B's closure declares it.
    std::fs::write(
        proj_a.join("project.gcl"),
        "fn onlyInA(): int { return 11; }\n",
    )
    .unwrap();
    std::fs::write(
        proj_b.join("project.gcl"),
        "fn rootB(): int { return 0; }\n",
    )
    .unwrap();

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

    write_msg(
        &mut stdin,
        &serde_json::json!({
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
        }),
    );
    let _ = read_msg(&mut stdout, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    );

    // Open a file in projB that references `onlyInA` (declared only
    // in projA). Goto-def must NOT point into projA.
    let caller_path = proj_b.join("caller.gcl");
    let caller_text = "fn caller() { onlyInA(); }\n";
    std::fs::write(&caller_path, caller_text).unwrap();
    let caller_uri = uri_for(&caller_path);
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": caller_uri,
                    "languageId": "greycat",
                    "version": 1,
                    "text": caller_text,
                }
            }
        }),
    );
    // Drain the publishDiagnostics for caller.gcl so the server has
    // finished its analyze pass for this file.
    let _ = read_until(
        &mut stdout,
        |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"].as_str() == Some(caller_uri.as_str())
        },
        Duration::from_secs(10),
    );

    // Send a goto-def request at the start of `onlyInA(...)`. The
    // string `onlyInA` begins at column 14 of `fn caller() { onlyInA(); }`.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": caller_uri },
                "position": { "line": 0, "character": 16 },
            }
        }),
    );
    let resp = read_until(&mut stdout, |m| m["id"] == 2, Duration::from_secs(5));
    let result_str = serde_json::to_string(&resp["result"]).unwrap();
    // Any non-null result MUST not reference projA.
    let proj_a_uri = uri_for(&proj_a);
    assert!(
        !result_str.contains(&proj_a_uri),
        "goto-def in projB leaked a location from projA: {result_str}"
    );

    // Tidy shutdown.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null
        }),
    );
    let _ = read_until(&mut stdout, |m| m["id"] == 99, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );
    drop(stdin);
    let _ = child.wait();
}

// P32.5
/// A `.gcl` file inside a workspace folder with no `project.gcl`
/// up-tree gets the "orphan-module" advisory: an Information +
/// UNNECESSARY diag spanning the whole file. No resolver / analyzer
/// output should ever appear for it.
#[test]
fn orphan_file_publishes_dim_diagnostic() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lsp_orphan");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    // Workspace folder is `base` itself — no project.gcl anywhere.
    let loose = base.join("loose.gcl");
    let loose_text = "fn loose(): int { return 0; }\n";
    std::fs::write(&loose, loose_text).unwrap();

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

    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "workspaceFolders": [
                    { "uri": uri_for(&base), "name": "ws" },
                ],
                "capabilities": {},
            }
        }),
    );
    let _ = read_msg(&mut stdout, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let loose_uri = uri_for(&loose);
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": loose_uri,
                    "languageId": "greycat",
                    "version": 1,
                    "text": loose_text,
                }
            }
        }),
    );

    let diags_msg = read_until(
        &mut stdout,
        |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"].as_str() == Some(loose_uri.as_str())
        },
        Duration::from_secs(10),
    );
    let diags = diags_msg["params"]["diagnostics"].as_array().unwrap();

    // Exactly one diagnostic, with the orphan-module code.
    assert_eq!(
        diags.len(),
        1,
        "orphan publish should contain exactly the dim diag, got: {diags:?}"
    );
    let d = &diags[0];
    assert_eq!(d["code"].as_str(), Some("orphan-module"));
    // Information severity = 3 in the LSP wire encoding.
    assert_eq!(d["severity"].as_i64(), Some(3));
    let tags = d["tags"].as_array().expect("tags array");
    // DiagnosticTag::UNNECESSARY = 1.
    assert!(
        tags.iter().any(|t| t.as_i64() == Some(1)),
        "orphan diag must carry UNNECESSARY tag, got: {tags:?}"
    );

    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null }),
    );
    let _ = read_until(&mut stdout, |m| m["id"] == 99, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );
    drop(stdin);
    let _ = child.wait();
}

// P32.6
/// A file reachable from two projects' `@include` closures gets the
/// `multi-project-owner` advisory in addition to the first owner's
/// analysis. The diag is Information severity with `UNNECESSARY` so
/// the editor dims the file.
#[test]
fn shared_file_publishes_multi_owner_diagnostic() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lsp_multi_owner");
    let _ = std::fs::remove_dir_all(&base);
    let shared = base.join("shared");
    let proj_a = base.join("projA");
    let proj_b = base.join("projB");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();

    let common = shared.join("common.gcl");
    std::fs::write(&common, "fn shared_fn(): int { return 0; }\n").unwrap();
    // Both projects include the shared dir via a `..` hop.
    std::fs::write(proj_a.join("project.gcl"), "@include(\"../shared\");\n").unwrap();
    std::fs::write(proj_b.join("project.gcl"), "@include(\"../shared\");\n").unwrap();

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

    write_msg(
        &mut stdin,
        &serde_json::json!({
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
        }),
    );
    let _ = read_msg(&mut stdout, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    let common_uri = uri_for(&common);
    // Collect ALL publishDiagnostics for common.gcl. The eager load
    // republishes after each project's analyze, so the LATEST one is
    // the post-second-owner state — that's the one we inspect.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_diags_for_common: Option<serde_json::Value> = None;
    let mut seen_after_both = false;
    while !seen_after_both && Instant::now() < deadline {
        let msg = read_msg(&mut stdout, Duration::from_secs(10));
        if msg["method"] == "textDocument/publishDiagnostics"
            && msg["params"]["uri"].as_str() == Some(common_uri.as_str())
        {
            let diags = msg["params"]["diagnostics"].clone();
            let has_multi = diags
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .any(|d| d["code"].as_str() == Some("multi-project-owner"))
                })
                .unwrap_or(false);
            if has_multi {
                seen_after_both = true;
            }
            last_diags_for_common = Some(diags);
        }
    }

    let diags = last_diags_for_common.expect("never saw a publish for shared/common.gcl");
    let arr = diags.as_array().unwrap();
    let multi = arr
        .iter()
        .find(|d| d["code"].as_str() == Some("multi-project-owner"))
        .unwrap_or_else(|| panic!("missing multi-project-owner diag in: {arr:?}"));
    assert_eq!(multi["severity"].as_i64(), Some(3));
    let tags = multi["tags"].as_array().expect("tags array");
    assert!(
        tags.iter().any(|t| t.as_i64() == Some(1)),
        "multi-owner diag must carry UNNECESSARY tag, got: {tags:?}"
    );
    let msg = multi["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("projA") && msg.contains("projB"),
        "diagnostic message should list both conflicting roots, got: {msg}"
    );

    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null }),
    );
    let _ = read_until(&mut stdout, |m| m["id"] == 99, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );
    drop(stdin);
    let _ = child.wait();
}

// P32.8
/// Adding a workspace folder at runtime eagerly loads its project;
/// removing one drops the project and clears diagnostics for its
/// previously-owned URIs.
#[test]
fn workspace_folders_add_then_remove() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lsp_ws_folders");
    let _ = std::fs::remove_dir_all(&base);
    let proj_a = base.join("projA");
    let proj_b = base.join("projB");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();
    let a_gcl = proj_a.join("project.gcl");
    let b_gcl = proj_b.join("project.gcl");
    std::fs::write(&a_gcl, "fn a(): int { return 1; }\n").unwrap();
    std::fs::write(&b_gcl, "fn b(): int { return 2; }\n").unwrap();

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

    // Start with only projA in the workspace.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "workspaceFolders": [
                    { "uri": uri_for(&proj_a), "name": "projA" },
                ],
                "capabilities": {},
            }
        }),
    );
    let _ = read_msg(&mut stdout, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );
    // Drain projA's eager publish so the next reads only contain
    // post-add/remove traffic.
    let entry_a = uri_for(&a_gcl);
    let _ = read_until(
        &mut stdout,
        |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"].as_str() == Some(entry_a.as_str())
        },
        Duration::from_secs(10),
    );

    // Add projB mid-session.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added":   [ { "uri": uri_for(&proj_b), "name": "projB" } ],
                    "removed": [],
                }
            }
        }),
    );
    let entry_b = uri_for(&b_gcl);
    let added_msg = read_until(
        &mut stdout,
        |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"].as_str() == Some(entry_b.as_str())
        },
        Duration::from_secs(10),
    );
    // Sanity: publish carries projB diagnostics (parse-clean, so the
    // diagnostics array may be empty — but the publish must exist).
    assert!(added_msg["params"]["diagnostics"].is_array());

    // Remove projA.
    write_msg(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeWorkspaceFolders",
            "params": {
                "event": {
                    "added":   [],
                    "removed": [ { "uri": uri_for(&proj_a), "name": "projA" } ],
                }
            }
        }),
    );
    // Removing a folder must clear-publish (empty diagnostics) for
    // every URI it owned. We assert this for projA's entrypoint.
    let clear_msg = read_until(
        &mut stdout,
        |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"].as_str() == Some(entry_a.as_str())
        },
        Duration::from_secs(10),
    );
    let diags = clear_msg["params"]["diagnostics"].as_array().unwrap();
    assert!(
        diags.is_empty(),
        "removing projA must clear-publish its entrypoint, got: {diags:?}"
    );

    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": null }),
    );
    let _ = read_until(&mut stdout, |m| m["id"] == 99, Duration::from_secs(5));
    write_msg(
        &mut stdin,
        &serde_json::json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );
    drop(stdin);
    let _ = child.wait();
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
