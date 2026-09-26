use crate::indexer::WorkspaceIndexer;
use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::{GraphRenderer, Topology, TopologyOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VisualizeMeshArgs {
    #[serde(default = "default_format")]
    #[schemars(
        with = "String",
        description = "Output format: 'mermaid' (default, GitHub-compatible Mermaid Markdown), 'json' (aggregated graph payload) or 'html' (standalone page). Every format renders the per-service aggregated view, never the raw contract graph."
    )]
    pub format: Option<CompactStr>,

    #[serde(default)]
    #[schemars(
        with = "Option<String>",
        description = "Zoom into one service (a name shown in the default view or listed as hidden in its footer): draws that service's contracts and the services they talk to. Omit for the whole-mesh service view."
    )]
    pub service: Option<CompactStr>,

    #[serde(default)]
    #[schemars(
        description = "Most services/topics drawn before the rest fold into one 'other' node (1-200, default 40). The view shrinks further on its own to stay under the 48 KB cap. DO NOT raise it to see everything; zoom with `service` instead."
    )]
    pub max_services: Option<u32>,

    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

fn default_format() -> Option<CompactStr> {
    Some(CompactStr::new("mermaid"))
}

const DEFAULT_MAX_SERVICES: u32 = 40;
const MAX_SERVICES_CAP: u32 = 200;
/// Rendered payload budget, under the 48 KB MCP cap with room for the footer.
const RENDER_BUDGET: usize = 44 * 1024;

pub struct VisualizeMeshTool;

impl McpTool for VisualizeMeshTool {
    const NAME: &'static str = "visualize_mesh";
    const DESCRIPTION: &'static str = "Generate a per-service architecture topology of the polyglot mesh (services, gRPC/HTTP contracts, topics, weighted cross-service links) in Mermaid, JSON or HTML. Zoom into one service with `service`. DO NOT USE for a targeted question about one symbol (use find_dependents / analyze_grpc).";
    type Args = VisualizeMeshArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(_args: &Self::Args, state: &AppState) -> Option<String> {
        let snapshot = state.snapshot();
        let graph = &snapshot.contract_graph;
        Some(format!(
            "Total graph nodes: {}, total edges: {}. Zoom with `service` or lower `max_services`.",
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
        if !matches!(format_choice.as_str(), "mermaid" | "json" | "html") {
            return Err((
                -32602,
                format!("Unknown format '{format_choice}': use 'mermaid', 'json' or 'html'."),
            ));
        }

        let snapshot = state.snapshot();
        let graph = &snapshot.contract_graph;
        let workspace_name = &state.config.workspace.name;
        let repo_names = WorkspaceIndexer::repo_names(&state.allowed_roots);

        let mut opts = TopologyOptions {
            max_groups: args
                .max_services
                .unwrap_or(DEFAULT_MAX_SERVICES)
                .clamp(1, MAX_SERVICES_CAP) as usize,
            focus: args.service.as_ref().map(|s| s.to_string()),
            ..TopologyOptions::default()
        };

        // Shrink until the rendering fits the budget: the same "show the top
        // groups, fold the rest, say how to zoom" contract `smart_search`'s
        // truncation follows, instead of cutting a document in half.
        loop {
            let topology = Topology::build(graph, &repo_names, &opts);
            if let (Some(asked), None) = (&opts.focus, &topology.focus) {
                return Err((
                    -32602,
                    format!(
                        "No service named '{asked}'. Call visualize_mesh without `service` to list them."
                    ),
                ));
            }
            let body = match format_choice.as_str() {
                "html" => GraphRenderer::payload_to_html(&topology.to_payload(workspace_name)),
                "json" => serde_json::to_string_pretty(&topology.to_payload(workspace_name))
                    .unwrap_or_else(|_| "{}".to_string()),
                _ => format!(
                    "## Polyglot Architecture Mesh Topology{}\n\n```mermaid\n{}```\n",
                    topology
                        .focus
                        .as_deref()
                        .map(|f| format!(" — service `{f}`"))
                        .unwrap_or_default(),
                    topology.to_mermaid(workspace_name)
                ),
            };
            let shrinkable = opts.max_groups > 1 || opts.max_focus_contracts > 1;
            if body.len() <= RENDER_BUDGET || !shrinkable {
                let text = match format_choice.as_str() {
                    // The page / document must stay parseable: no appended prose.
                    "html" | "json" => body,
                    _ => format!("{body}\n{}", topology.footer()),
                };
                return Ok(ToolOutput::text(text));
            }
            opts.max_groups = (opts.max_groups / 2).max(1);
            opts.max_focus_contracts = (opts.max_focus_contracts / 2).max(1);
        }
    }
}
