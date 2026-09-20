pub mod analyze_grpc;
pub mod analyze_impact;
pub mod find_dependents;
pub mod search_docs;
pub mod smart_search;

use analyze_grpc::{AnalyzeGrpcArgs, AnalyzeGrpcTool};
use analyze_impact::{AnalyzeImpactArgs, AnalyzeImpactTool};
use find_dependents::{FindDependentsArgs, FindDependentsTool};
use mesh_core::AppState;
use schemars::schema_for;
use search_docs::{SearchDocsArgs, SearchDocsTool};
use serde_json::{json, Value};
use smart_search::{SmartSearchArgs, SmartSearchTool};
use std::sync::Arc;

pub struct ToolRegistry;

impl ToolRegistry {
    /// Returns the schema definition for all 5 enterprise MCP tools
    pub fn list_tools() -> Value {
        let smart_search_schema = schema_for!(SmartSearchArgs);
        let find_dependents_schema = schema_for!(FindDependentsArgs);
        let analyze_grpc_schema = schema_for!(AnalyzeGrpcArgs);
        let analyze_impact_schema = schema_for!(AnalyzeImpactArgs);
        let search_docs_schema = schema_for!(SearchDocsArgs);

        json!([
            {
                "name": SmartSearchTool::NAME,
                "description": SmartSearchTool::DESCRIPTION,
                "inputSchema": smart_search_schema,
            },
            {
                "name": FindDependentsTool::NAME,
                "description": FindDependentsTool::DESCRIPTION,
                "inputSchema": find_dependents_schema,
            },
            {
                "name": AnalyzeGrpcTool::NAME,
                "description": AnalyzeGrpcTool::DESCRIPTION,
                "inputSchema": analyze_grpc_schema,
            },
            {
                "name": AnalyzeImpactTool::NAME,
                "description": AnalyzeImpactTool::DESCRIPTION,
                "inputSchema": analyze_impact_schema,
            },
            {
                "name": SearchDocsTool::NAME,
                "description": SearchDocsTool::DESCRIPTION,
                "inputSchema": search_docs_schema,
            }
        ])
    }

    /// Dispatches an incoming MCP tools/call request
    pub async fn call_tool(
        name: &str,
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<Value, (i32, String)> {
        let text_output = match name {
            SmartSearchTool::NAME => {
                let args: SmartSearchArgs = serde_json::from_value(arguments).map_err(|e| {
                    (
                        -32602,
                        format!("Invalid arguments for {}: {e}", SmartSearchTool::NAME),
                    )
                })?;
                SmartSearchTool::execute(args, state).await?
            }
            FindDependentsTool::NAME => {
                let args: FindDependentsArgs = serde_json::from_value(arguments).map_err(|e| {
                    (
                        -32602,
                        format!("Invalid arguments for {}: {e}", FindDependentsTool::NAME),
                    )
                })?;
                FindDependentsTool::execute(args, state).await?
            }
            AnalyzeGrpcTool::NAME => {
                let args: AnalyzeGrpcArgs = serde_json::from_value(arguments).map_err(|e| {
                    (
                        -32602,
                        format!("Invalid arguments for {}: {e}", AnalyzeGrpcTool::NAME),
                    )
                })?;
                AnalyzeGrpcTool::execute(args, state).await?
            }
            AnalyzeImpactTool::NAME => {
                let args: AnalyzeImpactArgs = serde_json::from_value(arguments).map_err(|e| {
                    (
                        -32602,
                        format!("Invalid arguments for {}: {e}", AnalyzeImpactTool::NAME),
                    )
                })?;
                AnalyzeImpactTool::execute(args, state).await?
            }
            SearchDocsTool::NAME => {
                let args: SearchDocsArgs = serde_json::from_value(arguments).map_err(|e| {
                    (
                        -32602,
                        format!("Invalid arguments for {}: {e}", SearchDocsTool::NAME),
                    )
                })?;
                SearchDocsTool::execute(args, state).await?
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_tools_contains_all_5() {
        let tools = ToolRegistry::list_tools();
        let arr = tools.as_array().expect("tools array");
        assert_eq!(arr.len(), 5);
        let names: Vec<_> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"smart_search"));
        assert!(names.contains(&"find_dependents"));
        assert!(names.contains(&"analyze_grpc"));
        assert!(names.contains(&"analyze_impact"));
        assert!(names.contains(&"search_docs"));
    }
}
