//! Gated smoke test: real FUSE mount + MCP exchange over a live server process.
//!
//! Only runs when LUNAR_SMOKE=1 is set. Plain `cargo test` (with or without
//! --features fuse) compiles the file and immediately returns without mounting or
//! starting any process.
//!
//! To run the full smoke suite:
//!   LUNAR_SMOKE=1 cargo test --features fuse --test smoke_real_mount
//!
//! The FUSE mount section requires macOS + --features fuse AND FUSE-T installed.
//! The MCP exchange section spawns `node` and requires the clients/mcp package to
//! have been built (`npm run build` inside clients/mcp).

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Top-level gate
// ---------------------------------------------------------------------------

#[test]
fn smoke_local_mode_real_mount_and_mcp() {
    if !common::smoke_enabled() {
        eprintln!("smoke_real_mount: skipped (set LUNAR_SMOKE=1 to enable)");
        return;
    }

    // Run FUSE mount section on macOS with --features fuse.
    #[cfg(all(target_os = "macos", feature = "fuse"))]
    {
        eprintln!("smoke_real_mount: starting FUSE mount smoke");
        run_fuse_mount_smoke();
    }

    #[cfg(not(all(target_os = "macos", feature = "fuse")))]
    {
        eprintln!(
            "smoke_real_mount: FUSE mount skipped \
             (requires macOS + --features fuse + FUSE-T installed)"
        );
    }

    // Run MCP exchange section (no fuse feature required).
    eprintln!("smoke_real_mount: starting MCP exchange smoke");
    run_mcp_exchange_smoke();
}

// ---------------------------------------------------------------------------
// FUSE mount smoke (macOS + --features fuse only)
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "macos", feature = "fuse"))]
fn run_fuse_mount_smoke() {
    let repo_dir = tempfile::tempdir().expect("create fixture repo dir");
    let mount_dir = tempfile::tempdir().expect("create mountpoint dir");

    // Write a sentinel file that proves the mount is serving the CAS.
    std::fs::write(repo_dir.path().join("smoke-sentinel.txt"), b"lunar-smoke-ok")
        .expect("write sentinel");
    std::fs::create_dir_all(repo_dir.path().join("sub"))
        .expect("create subdir");
    std::fs::write(repo_dir.path().join("sub").join("data.txt"), b"nested-file-content")
        .expect("write nested file");

    let bin = std::env::var("CARGO_BIN_EXE_lunar")
        .unwrap_or_else(|_| "lunar".to_string());

    let child = std::process::Command::new(&bin)
        .arg("mount")
        .arg(repo_dir.path())
        .arg(mount_dir.path())
        .spawn()
        .expect(
            "failed to spawn lunar mount \
             (ensure binary is built with --features fuse and FUSE-T is installed)",
        );

    let guard = FuseGuard {
        mountpoint: mount_dir.path().to_path_buf(),
        child: Some(child),
        _repo: repo_dir,
        _mount: mount_dir,
    };

    // Poll for mount readiness: up to 15 s at 200 ms intervals.
    let sentinel = guard.mountpoint.join("smoke-sentinel.txt");
    let mut ready = false;
    for _ in 0..75usize {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if std::fs::read(&sentinel)
            .map(|b| b == b"lunar-smoke-ok")
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
    }
    assert!(ready, "smoke_real_mount: FUSE mount never became ready within 15 s");
    eprintln!("smoke_real_mount: FUSE mount ready");

    // Verify sentinel content.
    let got = std::fs::read(guard.mountpoint.join("smoke-sentinel.txt"))
        .expect("sentinel read must succeed");
    assert_eq!(got, b"lunar-smoke-ok", "sentinel content must match");

    // Verify nested file.
    let nested = std::fs::read(guard.mountpoint.join("sub").join("data.txt"))
        .expect("nested file read must succeed");
    assert_eq!(nested, b"nested-file-content", "nested file content must match");

    eprintln!("smoke_real_mount: FUSE read assertions passed");
    // guard drops here: unmounts and kills child.
    drop(guard);
}

// RAII teardown guard for the FUSE mount child process.
#[cfg(all(target_os = "macos", feature = "fuse"))]
struct FuseGuard {
    mountpoint: PathBuf,
    child: Option<std::process::Child>,
    _repo: tempfile::TempDir,
    _mount: tempfile::TempDir,
}

#[cfg(all(target_os = "macos", feature = "fuse"))]
impl Drop for FuseGuard {
    fn drop(&mut self) {
        let mp = self.mountpoint.to_string_lossy().into_owned();
        let plain_ok = std::process::Command::new("umount")
            .arg(&self.mountpoint)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !plain_ok {
            let _ = std::process::Command::new("diskutil")
                .args(["unmount", "force", &mp])
                .status();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::process::Command::new("pkill")
            .args(["-f", &format!("go-nfsv4.*{}", mp)])
            .status();
    }
}

// ---------------------------------------------------------------------------
// MCP exchange smoke
// ---------------------------------------------------------------------------

fn run_mcp_exchange_smoke() {
    let mcp_script = match find_mcp_dist() {
        Some(p) => p,
        None => {
            eprintln!(
                "smoke_real_mount: lunar-mcp dist not found; skipping MCP exchange. \
                 Run `npm run build` in clients/mcp first."
            );
            return;
        }
    };

    // Spawn the lunar-mcp stdio server (node dist/index.js).
    let mut child = std::process::Command::new("node")
        .arg(&mcp_script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect(
            "failed to spawn node for lunar-mcp; \
             ensure node >= 22 is available in PATH",
        );

    let mut stdin = child.stdin.take().expect("stdin must be piped");
    let stdout = child.stdout.take().expect("stdout must be piped");
    let mut reader = BufReader::new(stdout);

    // MCP initialize request (Content-Length framing, as per the MCP SDK stdio transport).
    let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-test","version":"1.0"}}}"#;
    write_mcp_frame(&mut stdin, init_body);

    // Read initialize response.
    let init_response = read_mcp_frame(&mut reader);
    let init_val: serde_json::Value =
        serde_json::from_str(&init_response)
            .expect("MCP initialize response must be valid JSON");
    assert_eq!(
        init_val["id"],
        serde_json::json!(1),
        "initialize response id must match request id"
    );
    assert!(
        init_val["result"].is_object(),
        "initialize result must be an object, got: {:?}",
        init_val
    );
    let server_name = init_val["result"]["serverInfo"]["name"].as_str().unwrap_or("");
    assert!(
        !server_name.is_empty(),
        "serverInfo.name must be non-empty in initialize response"
    );
    eprintln!("smoke_real_mount: MCP initialize OK (server: {})", server_name);

    // MCP notifications/initialized (required by protocol before any tool calls).
    let notif_body = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    write_mcp_frame(&mut stdin, notif_body);

    // MCP tools/list request.
    let list_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    write_mcp_frame(&mut stdin, list_body);

    // Read tools/list response.
    let list_response = read_mcp_frame(&mut reader);
    let list_val: serde_json::Value =
        serde_json::from_str(&list_response)
            .expect("MCP tools/list response must be valid JSON");
    assert_eq!(
        list_val["id"],
        serde_json::json!(2),
        "tools/list response id must match request id"
    );
    let tools = list_val["result"]["tools"]
        .as_array()
        .expect("tools/list result must contain a 'tools' array");
    assert!(
        !tools.is_empty(),
        "lunar-mcp must expose at least one tool"
    );
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        tool_names.len() <= 64,
        "tool list must not exceed sanity cap of 64 entries"
    );
    eprintln!(
        "smoke_real_mount: MCP tools/list OK ({} tools: {:?})",
        tool_names.len(),
        tool_names
    );

    // Teardown.
    drop(stdin); // close stdin -> node process exits naturally
    let _ = child.wait();
    eprintln!("smoke_real_mount: MCP exchange smoke PASSED");
}

// ---------------------------------------------------------------------------
// MCP framing helpers (Content-Length, as used by the MCP SDK stdio transport)
// ---------------------------------------------------------------------------

fn write_mcp_frame(writer: &mut impl Write, body: &str) {
    assert!(!body.is_empty(), "MCP frame body must not be empty");
    assert!(body.len() <= 1_048_576, "MCP frame body exceeds 1 MiB sanity cap");
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).expect("MCP frame header write must succeed");
    writer.write_all(body.as_bytes()).expect("MCP frame body write must succeed");
    writer.flush().expect("MCP frame flush must succeed");
}

// Read one MCP frame: skip headers until the blank line, then read exactly
// Content-Length bytes. Times out via bounded line reads (no infinite block).
fn read_mcp_frame(reader: &mut impl BufRead) -> String {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    // Read headers until the blank \r\n line.
    for _ in 0..32usize {
        line.clear();
        reader.read_line(&mut line).expect("MCP response header read must succeed");
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let n: usize = rest.trim().parse().expect("Content-Length must be a valid usize");
            assert!(n <= 1_048_576, "MCP response body exceeds 1 MiB sanity cap");
            content_length = Some(n);
        }
    }

    let n = content_length.expect("MCP response must include a Content-Length header");
    assert!(n > 0, "MCP response Content-Length must be positive");

    let mut body = vec![0u8; n];
    let mut offset = 0usize;
    for _ in 0..1024usize {
        if offset >= n {
            break;
        }
        let read = reader
            .read(&mut body[offset..])
            .expect("MCP response body read must succeed");
        assert!(read > 0 || offset >= n, "MCP response body read returned 0 unexpectedly");
        offset += read;
    }
    assert_eq!(offset, n, "MCP response body must be exactly Content-Length bytes");
    String::from_utf8(body).expect("MCP response body must be valid UTF-8")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// Locate clients/mcp/dist/index.js relative to the workspace manifest dir.
fn find_mcp_dist() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR is set by cargo for integration test binaries.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let candidate = PathBuf::from(manifest)
        .join("clients")
        .join("mcp")
        .join("dist")
        .join("index.js");
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}
