pub mod analyze_grpc;
pub mod analyze_impact;
pub mod find_dependents;
pub mod search_docs;
pub mod smart_search;
pub mod visualize_mesh;

use crate::protocol::RequestMeta;
use analyze_grpc::AnalyzeGrpcTool;
use analyze_impact::AnalyzeImpactTool;
use find_dependents::FindDependentsTool;
use mesh_core::AppState;
use schemars::{schema_for, JsonSchema};
use search_docs::SearchDocsTool;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use smart_search::SmartSearchTool;
use std::sync::{Arc, LazyLock};
use visualize_mesh::VisualizeMeshTool;

/// JSON-RPC error tuple used throughout tool dispatch.
pub type ToolError = (i32, String);

/// Result of one tool invocation, before audit and MCP framing.
pub struct ToolOutput {
    pub text: String,
    pub files_accessed: Vec<String>,
    pub secrets_redacted: usize,
}

impl ToolOutput {
    pub fn text(text: String) -> Self {
        Self {
            text,
            files_accessed: Vec::new(),
            secrets_redacted: 0,
        }
    }
}

/// One MCP tool. `run` is synchronous on purpose: every tool touches the disk,
/// tree-sitter or SQLite, and the registry executes it on the blocking pool so
/// the JSON-RPC event loop (and, in daemon mode, other clients) never stalls.
pub trait McpTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + Serialize + JsonSchema + Send + 'static;

    fn meta(args: &Self::Args) -> Option<&RequestMeta>;
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError>;
}

pub struct ToolRegistry;

impl ToolRegistry {
    /// Returns the schema definition for all enterprise MCP tools.
    /// Built once: `schema_for!` walks the whole type graph and `tools/list` is
    /// called by every client on connect.
    pub fn list_tools() -> Value {
        static TOOLS: LazyLock<Value> = LazyLock::new(|| {
            json!([
                ToolRegistry::describe::<SmartSearchTool>(),
                ToolRegistry::describe::<FindDependentsTool>(),
                ToolRegistry::describe::<AnalyzeGrpcTool>(),
                ToolRegistry::describe::<AnalyzeImpactTool>(),
                ToolRegistry::describe::<SearchDocsTool>(),
                ToolRegistry::describe::<VisualizeMeshTool>(),
            ])
        });
        TOOLS.clone()
    }

    fn describe<T: McpTool>() -> Value {
        json!({
            "name": T::NAME,
            "description": T::DESCRIPTION,
            "inputSchema": schema_for!(T::Args),
        })
    }

    /// Dispatches an incoming MCP tools/call request
    pub async fn call_tool(
        name: &str,
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<Value, ToolError> {
        let text_output = match name {
            SmartSearchTool::NAME => Self::invoke::<SmartSearchTool>(arguments, state).await?,
            FindDependentsTool::NAME => {
                Self::invoke::<FindDependentsTool>(arguments, state).await?
            }
            AnalyzeGrpcTool::NAME => Self::invoke::<AnalyzeGrpcTool>(arguments, state).await?,
            AnalyzeImpactTool::NAME => Self::invoke::<AnalyzeImpactTool>(arguments, state).await?,
            SearchDocsTool::NAME => Self::invoke::<SearchDocsTool>(arguments, state).await?,
            VisualizeMeshTool::NAME => Self::invoke::<VisualizeMeshTool>(arguments, state).await?,
            unknown => return Err((-32601, format!("Unknown tool: {unknown}"))),
        };

        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": text_output
                }
            ]
        }))
    }

    /// Parses arguments, runs the tool body and the audit write on the blocking
    /// pool, and returns the rendered text.
    async fn invoke<T: McpTool>(
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<String, ToolError> {
        let args: T::Args = serde_json::from_value(arguments)
            .map_err(|e| (-32602, format!("Invalid arguments for {}: {e}", T::NAME)))?;

        tokio::task::spawn_blocking(move || {
            let result = T::run(&args, &state);
            let (status, files, redacted) = match &result {
                Ok(out) => ("SUCCESS", out.files_accessed.clone(), out.secrets_redacted),
                Err(_) => ("ERROR", Vec::new(), 0),
            };

            // Audit is best-effort and off the executor; the SQLite write never
            // gates the response but always happens on the same blocking thread.
            let trace_id = T::meta(&args).and_then(|m| m.extract_trace_id());
            if let Err(e) = state.audit.record_entry(
                "active-session",
                trace_id.as_deref(),
                T::NAME,
                &serde_json::to_string(&args).unwrap_or_default(),
                status,
                files,
                redacted,
            ) {
                tracing::warn!(target: "mesh::audit", "Audit write failed for {}: {e}", T::NAME);
            }

            result.map(|out| out.text)
        })
        .await
        .map_err(|e| (-32603, format!("Tool task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_tools_contains_all_tools() {
        let tools = ToolRegistry::list_tools();
        let arr = tools.as_array().expect("tools array");
        assert_eq!(arr.len(), 6);
        let names: Vec<_> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"smart_search"));
        assert!(names.contains(&"find_dependents"));
        assert!(names.contains(&"analyze_grpc"));
        assert!(names.contains(&"analyze_impact"));
        assert!(names.contains(&"search_docs"));
        assert!(names.contains(&"visualize_mesh"));
    }
}
