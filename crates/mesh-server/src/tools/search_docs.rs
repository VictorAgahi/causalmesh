use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchDocsArgs {
    #[schemars(
        with = "String",
        description = "Architectural concept, ADR, or RFC term to search for (ex: 'Scatter-Gather', 'Transactional Outbox', 'Neo4j')"
    )]
    pub query: CompactStr,

    #[serde(default = "default_max_sections")]
    #[schemars(description = "Maximum number of conceptual sections to return (default: 3).")]
    pub max_sections: Option<usize>,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

fn default_max_sections() -> Option<usize> {
    Some(3)
}

pub struct SearchDocsTool;

impl SearchDocsTool {
    pub const NAME: &'static str = "search_docs";
    pub const DESCRIPTION: &'static str = "Semantic keyword search across Markdown architecture documents (C4, ADRs, RFCs). DO NOT USE to search application source code (use smart_search).";

    pub async fn execute(
        args: SearchDocsArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let max_sec = args.max_sections.unwrap_or(3);
        let doc_index = state.doc_index.load();
        let sections = doc_index.search(args.query.as_str(), max_sec);

        let formatted = MarkdownFormatter::format_doc_sections(args.query.as_str(), &sections);

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
