//! UDS multiplexing server for meshd.
//!
//! Accepts N simultaneous IDE client connections over a Unix Domain Socket.
//! Each connection relays JSON-RPC frames to the shared AppState tool handlers
//! without allocating any additional graph, watcher, or crawler.

#[cfg(unix)]
pub use unix_impl::run_uds_server;

#[cfg(unix)]
mod unix_impl {
    use crate::idle::ClientCounter;
    use mesh_core::AppState;
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
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

    // ── Public API ────────────────────────────────────────────────────────────────

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

    // ── Per-client handler ────────────────────────────────────────────────────────

    async fn handle_client(
        stream: UnixStream,
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        counter: ClientCounter,
    ) {
        let (reader_half, mut writer_half) = stream.into_split();
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

    // ── JSON-RPC dispatcher ───────────────────────────────────────────────────────

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

    // ── Tests ─────────────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::idle::ClientCounter;
        use mesh_core::{AuditLogger, BackgroundRescanEngine, Config};
        use std::sync::Arc;
        use tempfile::tempdir;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

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

        /// Definition of done for ROADMAP item 8: a deliberately slow tool
        /// call on one client must not delay a `ping` from another client.
        /// This is structurally guaranteed by `spawn_blocking` inside
        /// `ToolRegistry::invoke`, but nothing previously verified it — a
        /// future refactor could silently drop the `spawn_blocking` and
        /// regress the daemon to head-of-line blocking across clients.
        #[tokio::test]
        async fn test_slow_tool_call_does_not_block_other_client_ping() {
            let dir = tempdir().unwrap();
            let sock_path = dir.path().join("test-meshd-nonblock.sock");

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

            // Client A: a deliberately slow tool call (real ToolRegistry
            // dispatch, real spawn_blocking, real blocking sleep).
            let path_a = sock_path.clone();
            let slow_start = std::time::Instant::now();
            let slow_handle = tokio::spawn(async move {
                let mut stream = UnixStream::connect(&path_a).await.unwrap();
                let req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                            \"params\":{\"name\":\"test_slow_op\",\"arguments\":{\"sleep_ms\":800}}}\n";
                stream.write_all(req.as_bytes()).await.unwrap();
                let mut buf = vec![0u8; 512];
                let n = stream.read(&mut buf).await.unwrap();
                std::str::from_utf8(&buf[..n]).unwrap().to_string()
            });

            // Give the slow call time to actually be in flight on the
            // blocking pool before client B pings.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;

            // Client B: ping, issued while A's slow call is still running.
            let mut stream_b = UnixStream::connect(&sock_path).await.unwrap();
            let ping = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
            let ping_start = std::time::Instant::now();
            stream_b.write_all(ping.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; 256];
            let n = stream_b.read(&mut buf).await.unwrap();
            let ping_elapsed = ping_start.elapsed();
            let ping_resp = std::str::from_utf8(&buf[..n]).unwrap().to_string();

            assert!(
                ping_resp.contains("\"result\""),
                "ping should succeed: {ping_resp}"
            );
            assert!(
                ping_elapsed < std::time::Duration::from_millis(400),
                "ping must complete well before the slow call finishes, took {ping_elapsed:?}"
            );

            let slow_resp = slow_handle.await.unwrap();
            assert!(
                slow_resp.contains("\"result\""),
                "slow call should eventually succeed: {slow_resp}"
            );
            assert!(
                slow_start.elapsed() >= std::time::Duration::from_millis(800),
                "slow call must have actually taken the full sleep duration"
            );
            assert!(
                ping_elapsed < slow_start.elapsed(),
                "ping ({ping_elapsed:?}) must complete before the slow call ({:?})",
                slow_start.elapsed()
            );

            token.cancel();
        }

        /// Two `mesh-mcp run` processes racing to auto-spawn `meshd` both
        /// end up binding the same socket path. Exactly one bind must win;
        /// the other must fail cleanly (not hang, not corrupt state) so the
        /// losing process can fall back to connecting as a client instead.
        #[tokio::test]
        async fn test_two_daemons_racing_to_bind_only_one_wins() {
            let dir = tempdir().unwrap();
            let sock_path = dir.path().join("test-meshd-race.sock");

            let state = make_state();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            let counter_a = ClientCounter::new();
            let counter_b = ClientCounter::new();

            let path_a = sock_path.clone();
            let path_b = sock_path.clone();
            let state_a = state.clone();
            let state_b = state.clone();
            let token_a2 = token_a.clone();
            let token_b2 = token_b.clone();

            let handle_a = tokio::spawn(async move {
                run_uds_server(&path_a, state_a, token_a2, counter_a).await
            });
            let handle_b = tokio::spawn(async move {
                run_uds_server(&path_b, state_b, token_b2, counter_b).await
            });

            let (res_a, res_b) = tokio::join!(
                tokio::time::timeout(std::time::Duration::from_millis(400), handle_a),
                tokio::time::timeout(std::time::Duration::from_millis(400), handle_b),
            );

            let a_kept_running = res_a.is_err();
            let b_kept_running = res_b.is_err();
            assert_ne!(
                a_kept_running, b_kept_running,
                "exactly one of the two racing daemons should keep the listener \
                 (win the bind) while the other fails immediately"
            );

            if a_kept_running {
                token_a.cancel();
                let loser = res_b.unwrap().unwrap();
                assert!(
                    loser.is_err(),
                    "loser daemon must fail to bind the already-taken socket path"
                );
            } else {
                token_b.cancel();
                let loser = res_a.unwrap().unwrap();
                assert!(
                    loser.is_err(),
                    "loser daemon must fail to bind the already-taken socket path"
                );
            }
        }

        /// An idle-timeout (or SIGINT) firing mid-request must not drop the
        /// in-flight response: cancellation is only observed by the
        /// per-client loop between reads, so a request already dispatched
        /// must still complete and be written back to the client.
        #[tokio::test]
        async fn test_inflight_request_completes_despite_idle_cancellation() {
            let dir = tempdir().unwrap();
            let sock_path = dir.path().join("test-meshd-idle-midreq.sock");

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
            let req = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"test_slow_op\",\"arguments\":{\"sleep_ms\":300}}}\n";
            stream.write_all(req.as_bytes()).await.unwrap();

            // Fire the "idle watchdog"/shutdown signal while the request is
            // still executing on the blocking pool.
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            token.cancel();

            let mut buf = vec![0u8; 512];
            let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                .await
                .expect("read must not hang after cancellation")
                .unwrap();
            let resp = std::str::from_utf8(&buf[..n]).unwrap().to_string();

            assert!(
                resp.contains("\"result\""),
                "in-flight request must still complete and be returned to the client, got: {resp}"
            );
        }
    }
}

#[cfg(not(unix))]
use crate::idle::ClientCounter;
#[cfg(not(unix))]
use mesh_core::AppState;
#[cfg(not(unix))]
use std::sync::Arc;
#[cfg(not(unix))]
use tokio_util::sync::CancellationToken;

#[cfg(not(unix))]
pub async fn run_uds_server(
    _socket_path: &std::path::Path,
    _state: Arc<AppState>,
    _cancel_token: CancellationToken,
    _counter: ClientCounter,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("meshd background UDS daemon is only supported on Unix targets. On Windows, run `mesh-mcp run --standalone`.".into())
}
