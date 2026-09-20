use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

impl AnalyzeImpactTool {
    pub const NAME: &'static str = "analyze_impact";
    pub const DESCRIPTION: &'static str = "Maps asynchronous events, Kafka topics, queues, post-processors, and sagas. DO NOT USE for synchronous direct HTTP/gRPC RPC calls (use analyze_grpc).";

    pub async fn execute(
        args: AnalyzeImpactArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let graph = state.contract_graph.load();
        let flow = graph.analyze_impact(args.target.as_str());

        let formatted = MarkdownFormatter::format_impact_flow(&flow);

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

        Ok(formatted)
    }
}
