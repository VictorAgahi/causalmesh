use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeImpactArgs {
    #[schemars(
        with = "String",
        description = "Name of event (ex: 'EVENT_CREATED', 'event.created'), Kafka topic, queue, stream, post-processor class, or saga to analyze."
    )]
    pub target: CompactStr,

    #[serde(default)]
    #[schemars(
        with = "Option<u8>",
        description = "How many causal hops to traverse past the direct producers/consumers/topics of `target` (default: 1, direct only). Each extra hop follows a real graph edge — a transitive consumer that itself produces onto another topic pulls in that topic's own consumers too — not another text search. Clamped to 5."
    )]
    pub depth: Option<u8>,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

pub struct AnalyzeImpactTool;

impl McpTool for AnalyzeImpactTool {
    const NAME: &'static str = "analyze_impact";
    const DESCRIPTION: &'static str = "Maps asynchronous events, Kafka topics, queues, post-processors, and sagas — direct hits by default, or transitively through `depth` causal hops of real Produces/Consumes edges. DO NOT USE for synchronous direct HTTP/gRPC RPC calls (use analyze_grpc).";
    type Args = AnalyzeImpactArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, _state: &AppState) -> Option<String> {
        Some(format!(
            "Impact flow for target '{}' exceeds payload budget. Consider querying a specific downstream service or event name.",
            args.target
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let depth = args.depth.map(|d| d as usize).unwrap_or(1);
        let flow = snapshot
            .contract_graph
            .analyze_impact_with_depth(args.target.as_str(), depth);
        Ok(ToolOutput::text(MarkdownFormatter::format_impact_flow(
            &flow,
        )))
    }
}
