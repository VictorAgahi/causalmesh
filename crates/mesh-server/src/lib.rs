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
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;

/// Main MCP JSON-RPC stdio event loop per RFC-001 Rev. 2.9.0
pub async fn run_server(
    state: Arc<AppState>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tx_out, mut rx_in, writer_done) = StdioFramingActor::spawn(cancel_token.clone());

    tracing::info!(target: "mesh::server", "MeshMCP server initialized on stdio");

    while let Some(line) = rx_in.recv().await {
        let response = match protocol::classify(&line) {
            Incoming::Request(req) => respond(req, &state, false).await,
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
pub async fn respond(
    req: JsonRpcRequest,
    state: &Arc<AppState>,
    still_indexing: bool,
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
            if still_indexing {
                return JsonRpcResponse::success(
                    req_id,
                    ToolRegistry::tool_result(
                        "meshd is still indexing this workspace; retry shortly".to_string(),
                        true,
                    ),
                );
            }
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match ToolRegistry::call_tool(tool_name, arguments, state.clone()).await {
                Ok(call_result) => JsonRpcResponse::success(req_id, call_result),
                Err((code, msg)) => JsonRpcResponse::error(req_id, code, msg, None),
            }
        }
        unknown => {
            JsonRpcResponse::error(req_id, -32601, format!("Method not found: {unknown}"), None)
        }
    }
}
