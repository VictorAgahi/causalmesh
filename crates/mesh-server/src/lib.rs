#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
pub mod framing;
pub mod indexer;
pub mod protocol;
pub mod tools;
pub mod watcher;

pub use indexer::WorkspaceIndexer;
pub use watcher::FileWatcherService;

use framing::StdioFramingActor;
use mesh_core::AppState;
use protocol::{Incoming, JsonRpcRequest, JsonRpcResponse};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;

/// A fresh audit session id: `<unix-ms>-p<pid>-c<n>`, unique per process and
/// per call. `meshd` takes one per accepted client connection (one connection
/// = one agent session, since each `mesh-mcp run` proxy opens exactly one);
/// the standalone stdio server uses [`process_session_id`].
pub fn new_session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!(
        "{millis}-p{}-c{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// This process's own session id, created on first use.
pub fn process_session_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(new_session_id)
}

/// Main MCP JSON-RPC stdio event loop per RFC-001 Rev. 2.9.0
pub async fn run_server(
    state: Arc<AppState>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tx_out, mut rx_in, writer_done) = StdioFramingActor::spawn(cancel_token.clone());

    tracing::info!(target: "mesh::server", "MeshMCP server initialized on stdio");

    while let Some(line) = rx_in.recv().await {
        let response = match protocol::classify(&line) {
            Incoming::Request(req) => respond(req, &state, false, process_session_id()).await,
            Incoming::Notification(note) => {
                // JSON-RPC 2.0 §4.1: never answer a notification, not even
                // with an error (`notifications/initialized`, cancellations, ...).
                tracing::debug!(target: "mesh::server", method = %note.method, "notification");
                continue;
            }
            Incoming::Reject(err) => err,
        };

        let resp_json = serde_json::to_string(&response)?;
        if tx_out.send(resp_json).await.is_err() {
            break;
        }
    }

    // Drop tx_out so the writer task sees channel closure and flushes, then
    // wait for it to complete before returning — this is the key drain barrier.
    drop(tx_out);
    let _ = writer_done.await;

    Ok(())
}

/// Answers one JSON-RPC request (never a notification — see [`protocol::classify`]).
/// Shared by the stdio server and `meshd`'s IPC dispatcher so both speak exactly
/// the same protocol.
///
/// `still_indexing` (meshd before its first snapshot) turns `tools/call` into a
/// tool error the agent can retry on, instead of answering from an empty graph.
/// `session_id` is what the call is audited under (see [`new_session_id`]).
pub async fn respond(
    req: JsonRpcRequest,
    state: &Arc<AppState>,
    still_indexing: bool,
    session_id: &str,
) -> JsonRpcResponse {
    let req_id = req.id;
    match req.method.as_str() {
        "initialize" => JsonRpcResponse::success(
            req_id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "mesh-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        ),
        "ping" => JsonRpcResponse::success(req_id, json!({})),
        "tools/list" => {
            JsonRpcResponse::success(req_id, json!({ "tools": ToolRegistry::list_tools() }))
        }
        "tools/call" => {
            let Some(params) = req.params else {
                return JsonRpcResponse::error(
                    req_id,
                    -32602,
                    "Missing params for tools/call",
                    None,
                );
            };
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // Plan 4 step 4.12e: checked before `still_indexing` below, so an
            // unknown tool name during the first scan gets the same protocol
            // error it would once indexed, not a retry-able "still indexing"
            // that would never stop being wrong for that name.
            if !ToolRegistry::is_known_tool(tool_name) {
                return JsonRpcResponse::error(
                    req_id,
                    -32602,
                    format!("Unknown tool: {tool_name}"),
                    None,
                );
            }
            if still_indexing {
                return JsonRpcResponse::success(
                    req_id,
                    ToolRegistry::tool_result(
                        "meshd is still indexing this workspace; retry shortly".to_string(),
                        true,
                    ),
                );
            }
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match ToolRegistry::call_tool_in_session(
                tool_name,
                arguments,
                state.clone(),
                session_id,
            )
            .await
            {
                Ok(call_result) => JsonRpcResponse::success(req_id, call_result),
                Err((code, msg)) => JsonRpcResponse::error(req_id, code, msg, None),
            }
        }
        unknown => {
            JsonRpcResponse::error(req_id, -32601, format!("Method not found: {unknown}"), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{AuditLogger, BackgroundRescanEngine, Config};

    fn test_state() -> Arc<AppState> {
        let cfg =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        Arc::new(AppState::new(cfg, vec![], audit, rescan))
    }

    fn tools_call(name: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: Some("2.0".to_string()),
            id: Some(json!(1)),
            method: "tools/call".to_string(),
            params: Some(json!({ "name": name, "arguments": {} })),
        }
    }

    /// Regression (step 4.12e review): an unknown tool name during the first
    /// scan (`still_indexing: true`) got the generic, retry-able "still
    /// indexing" answer instead of the `-32602` it would get once indexed —
    /// a name that will never exist stayed silently wrong forever. The tool
    /// name is now checked before the `still_indexing` gate.
    #[tokio::test]
    async fn unknown_tool_is_rejected_even_while_still_indexing() {
        let resp = respond(tools_call("not_a_real_tool"), &test_state(), true, "s").await;
        let err = resp.error.expect("unknown tool must be a protocol error");
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("not_a_real_tool"), "{err:?}");
    }

    /// A known tool name during the first scan keeps the existing, retry-able
    /// "still indexing" behavior (not a regression of the reordering above).
    #[tokio::test]
    async fn known_tool_still_gets_the_still_indexing_retry_while_indexing() {
        let resp = respond(tools_call("smart_search"), &test_state(), true, "s").await;
        let result = resp
            .result
            .expect("known tool while indexing is a tool result, not an error");
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("still indexing"), "{text}");
        assert_eq!(result["isError"], json!(true));
    }

    #[test]
    fn session_ids_are_unique_and_the_process_id_is_stable() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert!(a.contains(&format!("-p{}-c", std::process::id())), "{a}");
        assert_eq!(process_session_id(), process_session_id());
    }

    /// Plan 4 step 4.8: every audited tool call (success, tool error, refused
    /// arguments) gets a latency row joined to its audit entry, and building the
    /// state records one process start — the data `mesh-mcp stats` reads.
    #[tokio::test]
    async fn tool_calls_persist_latency_and_state_records_process_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("audit.db");
        let cfg =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        let audit = Arc::new(AuditLogger::new(Some(db.clone())).expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, vec![], audit, rescan));

        // Refused arguments (unknown field) and a regular call.
        let _ = respond(tools_call("smart_search"), &state, false, "session-a").await;
        let mut ok = tools_call("smart_search");
        ok.params = Some(json!({ "name": "smart_search", "arguments": { "query": "x" } }));
        let _ = respond(ok, &state, false, "session-b").await;

        let entries = AuditLogger::read_entries(&db, None).expect("entries");
        let metrics = AuditLogger::read_metrics(&db, None).expect("metrics");
        let latencies = metrics.tool_latencies_us.expect("latency table");
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(latencies.len(), 2, "{latencies:?}");
        assert!(latencies.iter().all(|(tool, _)| tool == "smart_search"));
        // Each call is audited under the session it came from, not a constant.
        let sessions: Vec<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
        assert_eq!(sessions, ["session-a", "session-b"]);
        assert_eq!(metrics.process_starts.map(|s| s.len()), Some(1));
    }
}
