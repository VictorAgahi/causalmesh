use crate::indexer::WorkspaceIndexer;
use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::GraphRenderer;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

fn default_format() -> Option<CompactStr> {
    Some(CompactStr::new("mermaid"))
}

pub struct VisualizeMeshTool;

impl McpTool for VisualizeMeshTool {
    const NAME: &'static str = "visualize_mesh";
    const DESCRIPTION: &'static str = "Generate an architecture topology graph of the polyglot mesh (services, contracts, gRPC endpoints, Kafka topics) in Mermaid or HTML format.";
    type Args = VisualizeMeshArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(_args: &Self::Args, state: &AppState) -> Option<String> {
        let snapshot = state.snapshot();
        let graph = &snapshot.contract_graph;
        Some(format!(
            "Total graph nodes: {}, total edges: {}.",
            graph.node_count(),
            graph.edge_count()
        ))
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let format_choice = args
            .format
            .as_deref()
            .unwrap_or("mermaid")
            .to_ascii_lowercase();

        let snapshot = state.snapshot();
        let graph = &snapshot.contract_graph;
        let workspace_name = &state.config.workspace.name;
        let repo_names = WorkspaceIndexer::repo_names(&state.allowed_roots);

        let output = match format_choice.as_str() {
            "html" => GraphRenderer::to_html(graph, workspace_name, &repo_names),
            "json" => GraphRenderer::to_json(graph, workspace_name, &repo_names),
            _ => {
                let mermaid_code = GraphRenderer::to_mermaid(graph, workspace_name, &repo_names);
                format!(
                    "## Polyglot Architecture Mesh Topology\n\n```mermaid\n{}\n```\n",
                    mermaid_code.trim()
                )
            }
        };

        Ok(ToolOutput::text(output))
    }
}
