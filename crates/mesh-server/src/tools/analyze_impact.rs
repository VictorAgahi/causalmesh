use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::routes::{display_route, find_routes, nearest_routes, RouteQuery};
use mesh_core::{AppState, CompactStr, EdgeKind};
use mesh_parsers::{MarkdownFormatter, RouteImpactEntry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeImpactArgs {
    #[schemars(
        with = "String",
        description = "What is changing: a proto/gRPC method (ex: 'ProcessPayment', 'PaymentService.ProcessPayment') or service, or an event (ex: 'EVENT_CREATED', 'event.created'), Kafka topic, queue, stream, consumer handler class, or saga; or an HTTP route written 'VERB /path' (ex: 'PATCH /events/:id/state'; verb in GET/POST/PUT/PATCH/DELETE/HEAD/OPTIONS, case-insensitive; ':id', '{id}', '<id>', '[id]' are the same parameter). A route target needs both the verb and a leading '/'."
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
    const DESCRIPTION: &'static str = "Authoritative causal impact matrix: for an HTTP route (`VERB /path`), its handler (file:line, root) and the graph edges on it, covering the handler only (HTTP clients are not linked across repos); for a proto/gRPC method or service, its server handlers and clients; for an event, topic, queue, stream or saga, who publishes it (producers, outbox writes) and who consumes or subscribes to it (consumer handlers, message broker and event bus listeners) (direct by default, or through `depth` causal hops of real Produces/Consumes edges). A 0 labeled 'Authoritative AST Scan' covers every indexed workspace root, so a broad grep re-scan rarely adds anything; every row gives a path:line to check, and rows marked heuristic or ambiguous deserve a targeted read. Each row is EXTERNAL (another workspace root than the contract owner's) or INTERNAL (same root) and carries the edge confidence (exact / heuristic / ambiguous). Paged with `limit`/`offset`. DO NOT USE for the generated-stub trace or the .proto wire-format check (use analyze_grpc). It does NOT report test coverage, and does NOT find the callers of an HTTP route.";
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
        if let Some(route) = RouteQuery::parse_target(args.target.as_str()) {
            return Ok(ToolOutput::text(Self::route_impact(
                args,
                &route,
                state,
                &snapshot.contract_graph,
            )));
        }
        let mut matrix = snapshot
            .contract_graph
            .impact_matrix(args.target.as_str(), depth);
        let before = matrix.rows.len();
        if !args.include_tests.unwrap_or(false) {
            matrix
                .rows
                .retain(|r| !mesh_core::is_test_node(r.node, &state.allowed_roots));
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

impl AnalyzeImpactTool {
    /// `VERB /path` target: the handler(s) the route resolves to, then the
    /// edges the graph holds on those nodes. Nothing is inferred beyond them.
    fn route_impact(
        args: &AnalyzeImpactArgs,
        route: &RouteQuery,
        state: &AppState,
        graph: &mesh_core::ContractGraph,
    ) -> String {
        let include_tests = args.include_tests.unwrap_or(false);
        let prefixes = &state.http_prefixes;
        // Impact is about the declared route: a fragment of one is not it, so
        // neither is it counted among the hidden test routes.
        let (hits, hidden_tests) = find_routes(
            graph,
            route,
            prefixes,
            None,
            include_tests,
            &state.allowed_roots,
            mesh_core::routes::RouteRank::Value,
        );
        if hits.is_empty() {
            let near: Vec<(String, &mesh_core::ContractNode)> = nearest_routes(
                graph,
                route,
                prefixes,
                include_tests,
                &state.allowed_roots,
                8,
            )
            .into_iter()
            .filter_map(|n| display_route(n, prefixes.text(n.repo_id)).map(|r| (r, n)))
            .collect();
            return MarkdownFormatter::format_route_impact(
                args.target.as_str(),
                &[],
                0,
                &near,
                hidden_tests,
                &state.allowed_roots,
            );
        }
        // Edges are looked up in one pass, only for the handlers the table lists.
        let shown: Vec<&mesh_core::ContractNode> = hits
            .iter()
            .take(MarkdownFormatter::MAX_ROUTE_ENTRIES)
            .map(|h| h.node)
            .collect();
        let mut edges = Self::edges_on(graph, &shown, &state.allowed_roots);
        let total = hits.len();
        let entries: Vec<RouteImpactEntry> = hits
            .iter()
            .take(MarkdownFormatter::MAX_ROUTE_ENTRIES)
            .map(|h| RouteImpactEntry {
                route: display_route(h.node, prefixes.text(h.node.repo_id))
                    .unwrap_or_else(|| h.node.name.to_string()),
                node: h.node,
                edges: edges.remove(&h.node.id).unwrap_or_default(),
            })
            .collect();
        MarkdownFormatter::format_route_impact(
            args.target.as_str(),
            &entries,
            total,
            &[],
            hidden_tests,
            &state.allowed_roots,
        )
    }

    /// Edges of the graph touching `node` (events it produces or consumes, RPCs
    /// it calls), one rendered line each. Import edges are left to `find_dependents`.
    fn edges_on(
        graph: &mesh_core::ContractGraph,
        nodes: &[&mesh_core::ContractNode],
        roots: &[std::path::PathBuf],
    ) -> std::collections::HashMap<mesh_core::NodeId, Vec<String>> {
        let wanted: std::collections::HashSet<mesh_core::NodeId> =
            nodes.iter().map(|n| n.id).collect();
        let mut by_node: std::collections::HashMap<mesh_core::NodeId, Vec<String>> =
            std::collections::HashMap::new();
        for e in graph.all_edges() {
            if e.kind == EdgeKind::Imports || !(wanted.contains(&e.from) || wanted.contains(&e.to))
            {
                continue;
            }
            for (own, arrow, other_id) in [(e.from, "->", e.to), (e.to, "<-", e.from)] {
                if !wanted.contains(&own) {
                    continue;
                }
                let Some(other) = graph.get_node(other_id) else {
                    continue;
                };
                let place = roots
                    .get(other.repo_id as usize)
                    .and_then(|r| other.file_path.strip_prefix(r).ok())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| other.file_path.display().to_string());
                by_node.entry(own).or_default().push(format!(
                    "{arrow} {:?} `{}` (`{}:{}`, {})",
                    e.kind,
                    other.name.replace('`', "'"),
                    place,
                    other.line_start,
                    e.confidence.label()
                ));
            }
        }
        for lines in by_node.values_mut() {
            lines.sort();
            lines.dedup();
        }
        by_node
    }
}
