#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Plan 4 step 4.13 (b), option (i): when `meshd` dies in the middle of a
//! proxied session, `mesh-mcp run` ends with a clean EOF on stdout and says why
//! on stderr. The "daemon" here is a plain `UnixListener` owned by the test,
//! bound on a short private `MESH_SOCKET_PATH` with an isolated `HOME`, so no
//! real `meshd` is spawned and the user's own daemon socket is never touched.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn mesh_mcp_bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    path.pop(); // release/ or debug/
    path.join("mesh-mcp")
}

#[test]
fn daemon_dying_mid_session_ends_with_a_logged_eof_not_a_hang() {
    let home = tempfile::tempdir().expect("home");
    let ws = tempfile::tempdir().expect("workspace");
    let config = ws.path().join("mesh-mcp.toml");
    std::fs::write(
        &config,
        "[workspace]\nname = \"stdio-test\"\nversion = \"0\"\nroots = [\".\"]\n",
    )
    .expect("config");
    // Short on purpose: macOS caps a socket path at 104 bytes.
    let sock = PathBuf::from(format!("/tmp/mesh-413-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind fake daemon");

    // Fake daemon: the first connection is `ensure_daemon_running`'s liveness
    // probe; the second is the proxy. It reads one request, then "crashes".
    let daemon = std::thread::spawn(move || {
        let (probe, _) = listener.accept().expect("probe");
        drop(probe);
        let (conn, _) = listener.accept().expect("proxy");
        let mut line = String::new();
        BufReader::new(&conn).read_line(&mut line).expect("request");
        drop(conn);
        drop(listener);
        line
    });

    let mut child = Command::new(mesh_mcp_bin())
        .arg("--config")
        .arg(&config)
        .arg("run")
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("MESH_SOCKET_PATH", &sock)
        .env("RUST_LOG", "info")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mesh-mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .expect("write request");
    stdin.flush().expect("flush");

    let request = daemon.join().expect("fake daemon");
    assert!(request.contains("\"ping\""), "request relayed: {request}");

    // stdin stays open: only the daemon's death may end the session.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().expect("kill our own mesh-mcp");
            panic!("mesh-mcp did not end after the daemon closed its connection");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    let out = child.wait_with_output().expect("output");
    let _ = std::fs::remove_file(&sock);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Exit status 1: the session ended abnormally, and the client sees EOF.
    assert_eq!(status.code(), Some(1), "{status}\n{stderr}");
    assert!(out.stdout.is_empty(), "no partial frame on stdout");
    assert!(
        stderr.contains("meshd closed the connection mid-session"),
        "cause logged on stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("restart the MCP server"),
        "stderr:\n{stderr}"
    );
}
