use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr};
use mesh_parsers::MarkdownFormatter;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

fn default_max_sections() -> Option<usize> {
    Some(3)
}

pub struct SearchDocsTool;

impl McpTool for SearchDocsTool {
    const NAME: &'static str = "search_docs";
    const DESCRIPTION: &'static str = "Semantic keyword search across Markdown architecture documents (C4, ADRs, RFCs). DO NOT USE to search application source code (use smart_search).";
    type Args = SearchDocsArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.query.as_str())
    }

    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let max_sec = args.max_sections.unwrap_or(3);
        let snapshot = state.snapshot();
        let sections = snapshot.doc_index.search(args.query.as_str(), max_sec);
        Ok(ToolOutput::text(MarkdownFormatter::format_doc_sections(
            args.query.as_str(),
            &sections,
        )))
    }
}
