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

        const MAX_OUTPUT: usize = 48 * 1024 - 1024;
        let final_output = if output.len() > MAX_OUTPUT {
            let mut cut_off = MAX_OUTPUT;
            while !output.is_char_boundary(cut_off) {
                cut_off -= 1;
            }
            format!(
                "{}\n\n> [!NOTE]\n> Graph output truncated to fit within maximum MCP output payload (48 KB). Total nodes: {}, total edges: {}.\n",
                &output[..cut_off],
                graph.node_count(),
                graph.edge_count()
            )
        } else {
            output
        };

        Ok(ToolOutput::text(final_output))
    }
}
