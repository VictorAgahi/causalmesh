use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

impl FindDependentsTool {
    pub const NAME: &'static str = "find_dependents";
    pub const DESCRIPTION: &'static str = "Resolves in-memory O(1) reverse dependency graph across packages and shared modules. DO NOT USE to search freeform text or method signatures (use smart_search).";

    pub async fn execute(
        args: FindDependentsArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let graph = state.contract_graph.load();
        let dependents = graph.find_dependents(args.target.as_str());

        let owned_nodes: Vec<_> = dependents.into_iter().cloned().collect();
        let formatted = MarkdownFormatter::format_dependents(args.target.as_str(), &owned_nodes);

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
