//! IPC multiplexing server for meshd.
//!
//! Accepts N simultaneous IDE client connections over a Unix Domain Socket
//! (Unix) or a named pipe (Windows). Each connection relays JSON-RPC frames
//! to the shared AppState tool handlers without allocating any additional
//! graph, watcher, or crawler.

use crate::idle::ClientCounter;
use mesh_core::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

// ── Minimal inline JSON-RPC types (avoids circular dep with mesh-server) ─────

#[derive(Debug, Deserialize)]
struct RpcRequest {
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ── JSON-RPC dispatcher (platform-independent) ───────────────────────────────

async fn dispatch(line: &str, state: &Arc<AppState>) -> Option<String> {
    use mesh_server::tools::ToolRegistry;

    let req: RpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let resp = RpcResponse::error(None, -32700, format!("Parse error: {e}"));
            return serde_json::to_string(&resp).ok();
        }
    };

    let id = req.id.clone();

    let response = match req.method.as_str() {
        "initialize" => RpcResponse::success(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "mesh-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "notifications/initialized" => return None,
        "ping" => RpcResponse::success(id, json!({})),
        "tools/list" => {
            let tools = ToolRegistry::list_tools();
            RpcResponse::success(id, json!({ "tools": tools }))
        }
        "tools/call" => {
            if let Some(params) = req.params {
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match ToolRegistry::call_tool(tool_name, arguments, state.clone()).await {
                    Ok(result) => RpcResponse::success(id, result),
                    Err((code, msg)) => RpcResponse::error(id, code.into(), msg),
                }
            } else {
                RpcResponse::error(id, -32602, "Missing params for tools/call")
            }
        }
        unknown => RpcResponse::error(id, -32601, format!("Method not found: {unknown}")),
    };

    serde_json::to_string(&response).ok()
}

// ── Per-client handler (generic over any duplex IPC stream) ─────────────────

/// Drives one client connection to completion: reads newline-delimited
/// JSON-RPC requests, dispatches them against the shared `AppState`, and
/// writes back newline-delimited responses. Generic over the underlying
/// transport so the same logic serves Unix Domain Sockets and Windows named
/// pipes without duplication.
async fn handle_client<S>(
    stream: S,
    state: Arc<AppState>,
    cancel_token: CancellationToken,
    counter: ClientCounter,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader_half, mut writer_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader_half);
    let mut line = String::new();

    loop {
        line.clear();
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            read_res = reader.read_line(&mut line) => {
                match read_res {
                    Ok(0) => break, // EOF — client disconnected
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Some(resp) = dispatch(trimmed, &state).await {
                            let mut out = resp;
                            out.push('\n');
                            if writer_half.write_all(out.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "meshd::server", "Read error: {e}");
                        break;
                    }
                }
            }
        }
    }

    counter.decrement();
}

// ── Unix Domain Socket transport ─────────────────────────────────────────────

#[cfg(unix)]
pub use unix_impl::run_uds_server;

#[cfg(unix)]
mod unix_impl {
    use super::{handle_client, ClientCounter};
    use mesh_core::AppState;
    use std::sync::Arc;
    use tokio::net::UnixListener;
    use tokio_util::sync::CancellationToken;

    /// Binds to `socket_path` and accepts connections until `cancel_token` fires.
    pub async fn run_uds_server(
        socket_path: &std::path::Path,
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        counter: ClientCounter,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = UnixListener::bind(socket_path)?;
        tracing::info!(
            target: "meshd::server",
            "meshd listening on {}",
            socket_path.display()
        );

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!(target: "meshd::server", "Shutdown signal received, closing listener.");
                    break;
                }
                accept_res = listener.accept() => {
                    match accept_res {
                        Ok((stream, _addr)) => {
                            let state = state.clone();
                            let cancel = cancel_token.clone();
                            let counter = counter.clone();
                            counter.increment();
                            tokio::spawn(async move {
                                handle_client(stream, state, cancel, counter).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(target: "meshd::server", "Accept error: {e}");
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ── Windows named pipe transport ─────────────────────────────────────────────
//
// meshd has no Unix Domain Socket on Windows, so IDE clients share the daemon
// over a named pipe instead (ROADMAP Item 13). Windows named pipe servers are
// single-instance: each accepted client owns one `NamedPipeServer`, and a
// fresh instance must be created before (or immediately after) accepting the
// next connection to keep listening.

#[cfg(windows)]
pub use windows_impl::run_named_pipe_server;

#[cfg(windows)]
mod windows_impl {
    use super::{handle_client, ClientCounter};
    use mesh_core::AppState;
    use std::sync::Arc;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio_util::sync::CancellationToken;

    fn create_instance(
        pipe_name: &str,
        first: bool,
    ) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .create(pipe_name)
    }

    /// Creates the named pipe at `pipe_name` and accepts connections until
    /// `cancel_token` fires.
    pub async fn run_named_pipe_server(
        pipe_name: &str,
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        counter: ClientCounter,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut server = create_instance(pipe_name, true)?;
        tracing::info!(
            target: "meshd::server",
            "meshd listening on named pipe {}",
            pipe_name
        );

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!(target: "meshd::server", "Shutdown signal received, closing listener.");
                    break;
                }
                connect_res = server.connect() => {
                    // Swap in a fresh instance immediately so the pipe keeps
                    // accepting new clients while this one is handled.
                    let connected = server;
                    server = create_instance(pipe_name, false)?;

                    match connect_res {
                        Ok(()) => {
                            let state = state.clone();
                            let cancel = cancel_token.clone();
                            let counter_cloned = counter.clone();
                            counter.increment();
                            tokio::spawn(async move {
                                handle_client(connected, state, cancel, counter_cloned).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(target: "meshd::server", "Named pipe connect error: {e}");
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ── Fallback for exotic platforms (neither unix nor windows) ────────────────

#[cfg(not(any(unix, windows)))]
pub async fn run_uds_server(
    _socket_path: &std::path::Path,
    _state: Arc<AppState>,
    _cancel_token: CancellationToken,
    _counter: ClientCounter,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("meshd background daemon has no supported IPC transport on this platform. Run `mesh-mcp run --standalone`.".into())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod unix_tests {
    use super::unix_impl::run_uds_server;
    use crate::idle::ClientCounter;
    use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine, Config};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio_util::sync::CancellationToken;

    fn make_state() -> Arc<AppState> {
        let config = Config::load_from_str(
            "[workspace]\nname = \"test\"\nversion = \"2.9.0\"\nroots = [\".\"]\n",
        )
        .unwrap();
        let audit = Arc::new(AuditLogger::new(None).unwrap());
        let rescan = Arc::new(BackgroundRescanEngine::new().unwrap());
        Arc::new(AppState::new(config, vec![], audit, rescan))
    }

    #[tokio::test]
    async fn test_daemon_binds_uds_socket() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test-meshd.sock");

        let state = make_state();
        let token = CancellationToken::new();
        let counter = ClientCounter::new();

        let sock_path_clone = sock_path.clone();
        let state_clone = state.clone();
        let token_clone = token.clone();
        let counter_clone = counter.clone();

        tokio::spawn(async move {
            run_uds_server(&sock_path_clone, state_clone, token_clone, counter_clone)
                .await
                .unwrap();
        });

        // Wait for socket to appear
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            sock_path.exists(),
            "Socket file must exist after daemon binds"
        );

        token.cancel();
    }

    #[tokio::test]
    async fn test_daemon_20_concurrent_clients() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test-meshd-concurrent.sock");

        let state = make_state();
        let token = CancellationToken::new();
        let counter = ClientCounter::new();

        let sock_path_srv = sock_path.clone();
        let state_srv = state.clone();
        let token_srv = token.clone();
        let counter_srv = counter.clone();

        tokio::spawn(async move {
            run_uds_server(&sock_path_srv, state_srv, token_srv, counter_srv)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Spawn 20 concurrent clients all sending a ping
        let mut handles = vec![];
        for _ in 0..20 {
            let path = sock_path.clone();
            handles.push(tokio::spawn(async move {
                let mut stream = UnixStream::connect(&path).await.unwrap();
                let ping = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
                stream.write_all(ping.as_bytes()).await.unwrap();
                let mut buf = vec![0u8; 256];
                let n = stream.read(&mut buf).await.unwrap();
                let resp = std::str::from_utf8(&buf[..n]).unwrap().to_string();
                assert!(
                    resp.contains("\"result\""),
                    "Expected result in response: {resp}"
                );
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        token.cancel();
    }

    #[tokio::test]
    async fn test_daemon_ping_pong() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test-meshd-ping.sock");

        let state = make_state();
        let token = CancellationToken::new();
        let counter = ClientCounter::new();

        let sock_path_srv = sock_path.clone();
        let state_srv = state.clone();
        let token_srv = token.clone();
        let counter_srv = counter.clone();

        tokio::spawn(async move {
            run_uds_server(&sock_path_srv, state_srv, token_srv, counter_srv)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\"}\n";
        stream.write_all(ping.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 512];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

        assert_eq!(resp["id"], 42);
        assert!(resp["result"].is_object());

        token.cancel();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::windows_impl::run_named_pipe_server;
    use crate::idle::ClientCounter;
    use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine, Config};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio_util::sync::CancellationToken;

    fn make_state() -> Arc<AppState> {
        let config = Config::load_from_str(
            "[workspace]\nname = \"test\"\nversion = \"2.9.0\"\nroots = [\".\"]\n",
        )
        .unwrap();
        let audit = Arc::new(AuditLogger::new(None).unwrap());
        let rescan = Arc::new(BackgroundRescanEngine::new().unwrap());
        Arc::new(AppState::new(config, vec![], audit, rescan))
    }

    #[tokio::test]
    async fn test_daemon_ping_pong_named_pipe() {
        let pipe_name = format!(r"\\.\pipe\mesh-mcp-test-{}", std::process::id());

        let state = make_state();
        let token = CancellationToken::new();
        let counter = ClientCounter::new();

        let pipe_name_srv = pipe_name.clone();
        let state_srv = state.clone();
        let token_srv = token.clone();
        let counter_srv = counter.clone();

        tokio::spawn(async move {
            run_named_pipe_server(&pipe_name_srv, state_srv, token_srv, counter_srv)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = ClientOptions::new().open(&pipe_name).unwrap();
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\"}\n";
        client.write_all(ping.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 512];
        let n = client.read(&mut buf).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();

        assert_eq!(resp["id"], 42);
        assert!(resp["result"].is_object());

        token.cancel();
    }
}
