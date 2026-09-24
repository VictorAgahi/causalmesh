use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDependentsArgs {
    #[schemars(
        with = "String",
        description = "Target contract name (ex: 'UserAuthRequest') or package identifier (ex: '@volontariapp/domain-user') to trace reverse dependencies for."
    )]
    pub target: CompactStr,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

pub struct FindDependentsTool;

impl McpTool for FindDependentsTool {
    const NAME: &'static str = "find_dependents";
    const DESCRIPTION: &'static str = "Resolves in-memory O(1) reverse dependency graph across packages, shared modules, gRPC services, and event streams. DO NOT USE to search freeform text or string literals (use smart_search or ripgrep).";
    type Args = FindDependentsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, state: &AppState) -> Option<String> {
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());
        Some(format!(
            "Target '{}' has {} dependent consumer(s). Consider searching for specific caller sub-packages or narrowing your query.",
            args.target,
            dependents.len()
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.target.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let snapshot = state.snapshot();
        let dependents = snapshot
            .contract_graph
            .find_dependents(args.target.as_str());
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
