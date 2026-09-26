//! IPC multiplexing server for meshd.
//!
//! Accepts N simultaneous IDE client connections over a Unix Domain Socket
//! (Unix) or a named pipe (Windows). Each connection relays JSON-RPC frames
//! to the shared AppState tool handlers without allocating any additional
//! graph, watcher, or crawler.

use crate::idle::ClientCounter;
use mesh_core::AppState;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

// ── JSON-RPC dispatcher (platform-independent) ───────────────────────────────

async fn dispatch(line: &str, state: &Arc<AppState>) -> Option<String> {
    use mesh_server::protocol::{classify, Incoming};

    let response = match classify(line) {
        // `generation` starts at 0 and is only ever bumped by
        // `install_snapshot`, which the initial ingestion task calls exactly
        // once when its first scan completes — so 0 uniquely means "meshd
        // hasn't finished indexing this workspace yet", never a legitimately
        // empty one. `respond` answers `tools/call` with a retryable tool error
        // then, which is what lets the socket accept connections (and
        // `initialize`/`ping` succeed) before ingestion finishes.
        Incoming::Request(req) => {
            let still_indexing = state.snapshot().generation == 0;
            mesh_server::respond(req, state, still_indexing).await
        }
        // JSON-RPC 2.0 §4.1: a notification never gets a reply.
        Incoming::Notification(_) => return None,
        Incoming::Reject(err) => err,
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
// over a named pipe instead. Windows named pipe servers are
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

    fn create_instance(pipe_name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
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
    use std::path::PathBuf;
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
        let state = Arc::new(AppState::new(config, vec![], audit, rescan));
        // Real meshd only starts serving `tools/call` once its initial ingestion
        // installs a snapshot (generation > 0, see `dispatch`'s readiness check);
        // these tests exercise post-ready behavior, so mark it ready up front.
        state.install_snapshot(mesh_core::MeshSnapshot::default());
        state
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

    /// Before the initial ingestion installs its first snapshot
    /// (`generation == 0`), `tools/call` must report "still indexing" instead
    /// of silently proceeding against the default, empty snapshot — a real
    /// answer that happens to be empty must never be indistinguishable from
    /// "not ready yet". `initialize`/`ping` are unaffected: only `tools/call`
    /// itself depends on the workspace actually being indexed.
    #[tokio::test]
    async fn test_tools_call_reports_still_indexing_before_first_snapshot() {
        let dir = tempdir().unwrap();
        let sock_path = dir.path().join("test-meshd-not-ready.sock");

        // Unlike `make_state()`, this state is never marked ready.
        let config = Config::load_from_str(
            "[workspace]\nname = \"test\"\nversion = \"2.9.0\"\nroots = [\".\"]\n",
        )
        .unwrap();
        let audit = Arc::new(AuditLogger::new(None).unwrap());
        let rescan = Arc::new(BackgroundRescanEngine::new().unwrap());
        let state = Arc::new(AppState::new(config, vec![], audit, rescan));
        assert_eq!(state.snapshot().generation, 0, "test setup: not yet ready");

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

        // `initialize` succeeds even though ingestion hasn't finished.
        let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n";
        stream.write_all(init.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        assert!(
            resp["result"].is_object(),
            "initialize must not block on ingestion"
        );

        // `tools/call` reports "still indexing" instead of an empty result.
        let call = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"smart_search\",\"arguments\":{}}}\n";
        stream.write_all(call.as_bytes()).await.unwrap();
        let n = stream.read(&mut buf).await.unwrap();
        let resp: serde_json::Value = serde_json::from_slice(&buf[..n]).unwrap();
        // A retryable tool error the agent can read, not a JSON-RPC error.
        assert!(resp["error"].is_null(), "{resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("indexing"));

        // A notification (no `id`) gets no reply at all: the next frame read
        // must be the answer to the ping sent right after it.
        let frames = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\"}\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n";
        stream.write_all(frames.as_bytes()).await.unwrap();
        let n = stream.read(&mut buf).await.unwrap();
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(text.lines().count(), 1, "exactly one reply: {text}");
        let resp: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(resp["id"], 3);

        token.cancel();
    }

    /// Two workspaces, each with its own `meshd` bound to its own
    /// `socket_path_for(workspace_id(..))`, must never answer for each
    /// other — a client connected to workspace A's socket must only ever
    /// see workspace A's data, however similarly the two are named or
    /// configured (idempotence invariant I7).
    #[tokio::test]
    async fn test_two_workspace_scoped_daemons_never_cross_talk() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let sock_a = mesh_core::socket_path_for(&mesh_core::workspace_id(dir_a.path()));
        let sock_b = mesh_core::socket_path_for(&mesh_core::workspace_id(dir_b.path()));
        assert_ne!(
            sock_a, sock_b,
            "two different workspaces must resolve to different sockets"
        );

        let mk = |name: &str| -> Arc<AppState> {
            let config = Config::load_from_str(&format!(
                "[workspace]\nname = \"{name}\"\nversion = \"2.9.0\"\nroots = [\".\"]\n"
            ))
            .unwrap();
            let audit = Arc::new(AuditLogger::new(None).unwrap());
            let rescan = Arc::new(BackgroundRescanEngine::new().unwrap());
            let state = Arc::new(AppState::new(config, vec![], audit, rescan));
            state.install_snapshot(mesh_core::MeshSnapshot::default());
            state
        };
        let state_a = mk("workspace-a");
        let state_b = mk("workspace-b");

        let token = CancellationToken::new();
        let counter = ClientCounter::new();
        for (sock, state) in [(&sock_a, state_a), (&sock_b, state_b)] {
            let sock = sock.clone();
            let state = state.clone();
            let token = token.clone();
            let counter = counter.clone();
            tokio::spawn(async move {
                run_uds_server(&sock, state, token, counter).await.unwrap();
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let query = |sock_path: PathBuf| async move {
            let mut stream = UnixStream::connect(&sock_path).await.unwrap();
            let req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"visualize_mesh\",\"arguments\":{\"format\":\"json\"}}}\n";
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        };

        let resp_a = query(sock_a).await;
        let resp_b = query(sock_b).await;

        assert!(
            resp_a.contains("workspace-a") && !resp_a.contains("workspace-b"),
            "workspace A's socket must only ever answer with workspace A's data, got: {resp_a}"
        );
        assert!(
            resp_b.contains("workspace-b") && !resp_b.contains("workspace-a"),
            "workspace B's socket must only ever answer with workspace B's data, got: {resp_b}"
        );

        token.cancel();
    }

    /// A deliberately slow tool call on one client must not delay a `ping`
    /// from another client.
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

        let handle_a =
            tokio::spawn(
                async move { run_uds_server(&path_a, state_a, token_a2, counter_a).await },
            );
        let handle_b =
            tokio::spawn(
                async move { run_uds_server(&path_b, state_b, token_b2, counter_b).await },
            );

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
        let state = Arc::new(AppState::new(config, vec![], audit, rescan));
        // Real meshd only starts serving `tools/call` once its initial ingestion
        // installs a snapshot (generation > 0, see `dispatch`'s readiness check);
        // these tests exercise post-ready behavior, so mark it ready up front.
        state.install_snapshot(mesh_core::MeshSnapshot::default());
        state
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
