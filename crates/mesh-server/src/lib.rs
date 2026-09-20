#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
pub mod framing;
pub mod protocol;
pub mod tools;
pub mod watcher;

pub use watcher::FileWatcherService;

use framing::StdioFramingActor;
use mesh_core::AppState;
use protocol::{JsonRpcRequest, JsonRpcResponse};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tools::ToolRegistry;

/// Main MCP JSON-RPC stdio event loop per RFC-001 Rev. 2.9.0
pub async fn run_server(
    state: Arc<AppState>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tx_out, mut rx_in) = StdioFramingActor::spawn(cancel_token.clone());

    tracing::info!(target: "mesh::server", "MeshMCP server initialized on stdio");

    while let Some(line) = rx_in.recv().await {
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp =
                    JsonRpcResponse::error(None, -32700, format!("Parse error: {e}"), None);
                let _ = tx_out.send(serde_json::to_string(&err_resp)?).await;
                continue;
            }
        };

        let req_id = req.id.clone();
        let method = req.method.as_str();

        let response = match method {
            "initialize" => {
                let init_result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "mesh-mcp",
                        "version": "2.9.0"
                    }
                });
                JsonRpcResponse::success(req_id, init_result)
            }
            "notifications/initialized" => {
                // Client initialized notification, no response required
                continue;
            }
            "ping" => JsonRpcResponse::success(req_id, json!({})),
            "tools/list" => {
                let tools_list = ToolRegistry::list_tools();
                JsonRpcResponse::success(req_id, json!({ "tools": tools_list }))
            }
            "tools/call" => {
                if let Some(params) = req.params {
                    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({}));

                    match ToolRegistry::call_tool(tool_name, arguments, state.clone()).await {
                        Ok(call_result) => JsonRpcResponse::success(req_id, call_result),
                        Err((code, msg)) => JsonRpcResponse::error(req_id, code, msg, None),
                    }
                } else {
                    JsonRpcResponse::error(req_id, -32602, "Missing params for tools/call", None)
                }
            }
            unknown => {
                JsonRpcResponse::error(req_id, -32601, format!("Method not found: {unknown}"), None)
            }
        };

        let resp_json = serde_json::to_string(&response)?;
        if tx_out.send(resp_json).await.is_err() {
            break;
        }
    }

    Ok(())
}
