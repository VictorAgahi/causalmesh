use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDependentsArgs {
    #[schemars(
        with = "String",
        description = "Target contract name (ex: 'UserAuthRequest') or package identifier (ex: '@volontariapp/domain-user') to trace reverse dependencies for."
    )]
    pub target: CompactStr,

    #[serde(default)]
    #[schemars(
        with = "String",
        description = "Result granularity: 'symbol' (default) returns one result per declaring symbol; 'package' collapses results to one per distinct (repo, package) pair — use this to see which *services* depend on the target without every individual caller symbol. Any other value is a JSON-RPC -32602 error, not a silent fallback to 'symbol'."
    )]
    pub granularity: Option<CompactStr>,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

/// Rejects an unrecognized `granularity` outright instead of silently falling
/// back to `"symbol"` — a typo'd or invented value (`"Package"`, `"packages"`)
/// would otherwise look like a successful narrower query while quietly
/// returning the full, undeduplicated result set.
fn validate_granularity(args: &FindDependentsArgs) -> Result<bool, ToolError> {
    match args.granularity.as_deref() {
        None | Some("symbol") => Ok(false),
        Some("package") => Ok(true),
        Some(other) => Err((
            -32602,
            format!(
                "Invalid granularity '{other}' for find_dependents: expected 'symbol' or 'package'."
            ),
        )),
    }
}

/// One result per distinct `(repo, package)` pair, preserving the input's
/// order (first symbol seen per package wins), so `run` and `truncation_hint`
/// dedupe identically instead of drifting apart.
fn dedup_by_package(dependents: Vec<&mesh_core::ContractNode>) -> Vec<&mesh_core::ContractNode> {
    let mut seen: HashSet<(mesh_core::RepoId, &str)> = HashSet::new();
    dependents
        .into_iter()
        .filter(|node| seen.insert((node.repo_id, node.package.as_str())))
        .collect()
}

pub struct FindDependentsTool;

impl McpTool for FindDependentsTool {
    const NAME: &'static str = "find_dependents";
    const DESCRIPTION: &'static str = "Resolves the reverse dependency graph across packages, shared modules, gRPC services, and event streams — at symbol granularity by default, or one result per (repo, package) with granularity: 'package'. DO NOT USE to search freeform text or string literals (use smart_search or ripgrep).";
    type Args = FindDependentsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, state: &AppState) -> Option<String> {
        // `run` already validated `granularity` (it must have succeeded for this
        // to be called at all) — recomputing here must dedupe the same way, or
        // this hint reports the pre-dedup symbol count for a `granularity:
        // "package"` request whose caller never saw that many results.
        let by_package = validate_granularity(args).ok()?;
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());
        let count = if by_package {
            dedup_by_package(dependents).len()
        } else {
            dependents.len()
        };
        Some(format!(
            "Target '{}' has {} dependent consumer(s). Consider searching for specific caller sub-packages or narrowing your query.",
            args.target,
            count
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let by_package = validate_granularity(args)?;
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());

        // `granularity: "package"` collapses every dependent down to one
        // result per distinct (repo, package) pair — useful for "which
        // *services* depend on this" without a wall of individual caller
        // symbols, several of which are very often declared in the same
        // package. Default ("symbol") keeps today's one-result-per-symbol
        // behavior unchanged; anything else was already rejected above.
        let dependents = if by_package {
            dedup_by_package(dependents)
        } else {
            dependents
        };

        // Label each result with the root it was crawled from so the
        // formatter can group same-named packages from unrelated services
        // apart instead of flattening them into one undifferentiated list.
        let labeled: Vec<(&_, String)> = dependents
            .into_iter()
            .map(|node| {
                let label = state
                    .allowed_roots
                    .get(node.repo_id as usize)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unknown root)".to_string());
                (node, label)
            })
            .collect();
        Ok(ToolOutput::text(MarkdownFormatter::format_dependents(
            args.target.as_str(),
            &labeled,
        )))
    }
}
