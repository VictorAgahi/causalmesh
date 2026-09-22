use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

impl McpTool for AnalyzeGrpcTool {
    const NAME: &'static str = "analyze_grpc";
    const DESCRIPTION: &'static str = "Traces end-to-end gRPC RPC definitions from .proto to polyglot generated stubs and controllers. DO NOT USE for message brokers or asynchronous event streams (use analyze_impact).";
    type Args = AnalyzeGrpcArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let trace = snapshot.contract_graph.analyze_grpc(args.target.as_str());
        Ok(ToolOutput::text(MarkdownFormatter::format_grpc_trace(
            &trace,
        )))
    }
}
