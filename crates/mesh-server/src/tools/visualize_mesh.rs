use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr};
use mesh_parsers::GraphRenderer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VisualizeMeshArgs {
    #[serde(default = "default_format")]
    #[schemars(
        with = "String",
        description = "Desired output format: 'mermaid' (returns GitHub-compatible Mermaid Markdown) or 'html' (returns standalone interactive HTML)."
    )]
    pub format: Option<CompactStr>,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

fn default_format() -> Option<CompactStr> {
    Some(CompactStr::new("mermaid"))
}

pub struct VisualizeMeshTool;

impl VisualizeMeshTool {
    pub const NAME: &'static str = "visualize_mesh";
    pub const DESCRIPTION: &'static str = "Generate an architecture topology graph of the polyglot mesh (services, contracts, gRPC endpoints, Kafka topics) in Mermaid or HTML format.";

    pub async fn execute(
        args: VisualizeMeshArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let format_choice = args
            .format
            .as_deref()
            .unwrap_or("mermaid")
            .to_ascii_lowercase();

        let graph = state.contract_graph.load();
        let config = state.config.load();
        let workspace_name = &config.workspace.name;

        let output = match format_choice.as_str() {
            "html" => GraphRenderer::to_html(&graph, workspace_name),
            "json" => GraphRenderer::to_json(&graph, workspace_name),
            _ => {
                let mermaid_code = GraphRenderer::to_mermaid(&graph, workspace_name);
                format!(
                    "## Polyglot Architecture Mesh Topology\n\n```mermaid\n{}\n```\n",
                    mermaid_code.trim()
                )
            }
        };

        let trace_id = args._meta.as_ref().and_then(|m| m.extract_trace_id());
        let _ = state.audit.record_entry(
            "active-session",
            trace_id.as_deref(),
            Self::NAME,
            &serde_json::to_string(&args).unwrap_or_default(),
            "SUCCESS",
            vec![],
            0,
        );

        Ok(output)
    }
}
