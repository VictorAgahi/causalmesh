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
        description = "What is changing: a proto/gRPC method (ex: 'ProcessPayment', 'PaymentService.ProcessPayment') or service, or an event (ex: 'EVENT_CREATED', 'event.created'), Kafka topic, queue, stream, consumer handler class, or saga."
    )]
    pub target: CompactStr,

    #[serde(default)]
    #[schemars(
        with = "Option<u8>",
        description = "How many causal hops to traverse past the direct producers/consumers/topics of `target` (default: 1, direct only). Each extra hop follows a real graph edge — a transitive consumer that itself produces onto another topic pulls in that topic's own consumers too — not another text search. Clamped to 5."
    )]
    pub depth: Option<u8>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Maximum number of matrix rows returned in this page (1-200, default 100). DO NOT raise it to see everything; page with `offset` instead."
    )]
    pub limit: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Number of matrix rows to skip (default 0). Use the `offset` value given in a previous page's 'More rows' footer."
    )]
    pub offset: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Include matches in test files and test-only directories (*.spec.ts, *_test.go, __tests__/, …). Default false: they are left out and counted."
    )]
    pub include_tests: Option<bool>,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

/// Rows per page when `limit` is omitted, and the most a caller may ask for.
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 200;

pub struct AnalyzeImpactTool;

impl McpTool for AnalyzeImpactTool {
    const NAME: &'static str = "analyze_impact";
    const DESCRIPTION: &'static str = "Authoritative causal impact matrix: for a proto/gRPC method or service, its server handlers and clients; for an event, topic, queue, stream or saga, who publishes it (producers, outbox writes) and who consumes or subscribes to it (consumer handlers, message broker and event bus listeners) (direct by default, or through `depth` causal hops of real Produces/Consumes edges). A 0 labeled 'Authoritative AST Scan' covers every indexed workspace root, so a broad grep re-scan rarely adds anything; every row gives a path:line to check, and rows marked heuristic or ambiguous deserve a targeted read. Each row is EXTERNAL (another workspace root than the contract owner's) or INTERNAL (same root) and carries the edge confidence (exact / heuristic / ambiguous). Paged with `limit`/`offset`. DO NOT USE for the generated-stub trace or the .proto wire-format check (use analyze_grpc). It does NOT report test coverage.";
    type Args = AnalyzeImpactArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, _state: &AppState) -> Option<String> {
        Some(format!(
            "Impact matrix for target '{}' exceeds payload budget. Page with `offset`, or query a fully-qualified method (`Service.Method`) or a specific event name.",
            args.target
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let depth = args.depth.map(|d| d as usize).unwrap_or(1);
        let limit = args
            .limit
            .map(|l| l.clamp(1, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT) as usize;
        let offset = args.offset.unwrap_or(0) as usize;
        let mut matrix = snapshot
            .contract_graph
            .impact_matrix(args.target.as_str(), depth);
        let before = matrix.rows.len();
        if !args.include_tests.unwrap_or(false) {
            matrix
                .rows
                .retain(|r| !mesh_core::is_test_path(&r.node.file_path));
        }
        let hidden_tests = before - matrix.rows.len();
        let mut text =
            MarkdownFormatter::format_impact_matrix(&matrix, &state.allowed_roots, offset, limit);
        if hidden_tests > 0 {
            text.push_str(&format!(
                "\n*{hidden_tests} row(s) in test files left out (`include_tests: true` to list them).*\n"
            ));
        }
        Ok(ToolOutput::text(text))
    }
}
