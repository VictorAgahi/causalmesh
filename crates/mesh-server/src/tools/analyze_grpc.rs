use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeGrpcArgs {
    #[schemars(
        with = "String",
        description = "Name of the gRPC service (ex: 'UserService'), RPC method (ex: 'SignUp', 'AuthenticateUser'), or package."
    )]
    pub target: CompactStr,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

pub struct AnalyzeGrpcTool;

impl AnalyzeGrpcTool {
    pub const NAME: &'static str = "analyze_grpc";
    pub const DESCRIPTION: &'static str = "Traces end-to-end gRPC RPC definitions from .proto to polyglot generated stubs and controllers. DO NOT USE for message brokers or asynchronous event streams (use analyze_impact).";

    pub async fn execute(
        args: AnalyzeGrpcArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let graph = state.contract_graph.load();
        let trace = graph.analyze_grpc(args.target.as_str());

        let formatted = MarkdownFormatter::format_grpc_trace(&trace);

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
