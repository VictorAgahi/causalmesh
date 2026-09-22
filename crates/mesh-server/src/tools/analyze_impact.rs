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
    pub _meta: Option<RequestMeta>,
}

pub struct AnalyzeImpactTool;

impl McpTool for AnalyzeImpactTool {
    const NAME: &'static str = "analyze_impact";
    const DESCRIPTION: &'static str = "Maps asynchronous events, Kafka topics, queues, post-processors, and sagas. DO NOT USE for synchronous direct HTTP/gRPC RPC calls (use analyze_grpc).";
    type Args = AnalyzeImpactArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let flow = snapshot.contract_graph.analyze_impact(args.target.as_str());
        Ok(ToolOutput::text(MarkdownFormatter::format_impact_flow(
            &flow,
        )))
    }
}
