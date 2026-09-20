use crate::protocol::RequestMeta;
use mesh_core::{AppState, CompactStr, FilesystemCrawler, ValidatedScope};
use mesh_parsers::{AstDecapitator, AstGuard, LanguageKind, MarkdownFormatter, SearchResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SmartSearchArgs {
    #[schemars(
        with = "String",
        description = "Symbol, class, or method declaration to search for across scoped repositories (ex: 'UserAuthRequest', 'createEvent')"
    )]
    pub query: CompactStr,

    #[schemars(
        with = "String",
        description = "Target repository or directory path (MANDATORY). Must resolve within configured allowed roots."
    )]
    pub scope: CompactStr,

    #[serde(default)]
    #[schemars(
        description = "If true, expands full implementation method bodies. Defaults to false (clean decapitated signatures)."
    )]
    pub include_body: bool,

    #[serde(default)]
    #[schemars(description = "Optional fuzzy fallback match if no exact match is found.")]
    pub fuzzy: Option<bool>,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

pub struct SmartSearchTool;

impl SmartSearchTool {
    pub const NAME: &'static str = "smart_search";
    pub const DESCRIPTION: &'static str = "Scans scoped repositories for symbol declarations and decapitated AST signatures. DO NOT USE to map package import hierarchies (use find_dependents).";

    pub async fn execute(
        args: SmartSearchArgs,
        state: Arc<AppState>,
    ) -> Result<String, (i32, String)> {
        let allowed_roots = state.allowed_roots.load();
        let validated_scope = match ValidatedScope::resolve(args.scope.as_str(), &allowed_roots) {
            Ok(s) => s,
            Err(e) => return Err((e.jsonrpc_code(), e.to_string())),
        };

        // Note: smart_search is a read-only discovery tool. Read access to guarded contract
        // scopes (such as proto-registry) is permitted so agents can inspect schemas and signatures.
        // Active governance (RSAH) is reserved for mutations and commit verification.

        let exclude_patterns = state.config.load().workspace.exclude_patterns.clone();
        let candidate_files =
            FilesystemCrawler::crawl_scope(&validated_scope, &exclude_patterns, Some(8));

        let query_str = args.query.as_str();
        let mut matches = Vec::new();
        let mut files_accessed = Vec::new();

        for file_path in candidate_files {
            let metadata = match fs::metadata(&file_path) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let content_bytes = match fs::read(&file_path) {
                Ok(b) => b,
                Err(_) => continue,
            };

            // Commandment 2: Lexical pre-check with path-aware schema sizing
            if !AstGuard::should_parse_path(&file_path, &metadata, &content_bytes) {
                continue;
            }

            let content_str = match std::str::from_utf8(&content_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if content_str.contains(query_str) {
                files_accessed.push(file_path.to_string_lossy().into_owned());
                let lang_kind = LanguageKind::from_path(&file_path.to_string_lossy());

                let decapitated =
                    AstDecapitator::decapitate_auto(content_str, lang_kind, args.include_body);

                // Find matching line ranges
                for (line_idx, line) in decapitated.lines().enumerate() {
                    if line.contains(query_str) {
                        let line_start = line_idx.saturating_sub(2) + 1;
                        let snippet_lines: Vec<&str> =
                            decapitated.lines().skip(line_start - 1).take(15).collect();
                        let line_end = line_start + snippet_lines.len() - 1;

                        matches.push(SearchResult {
                            file_path: file_path.to_string_lossy().into_owned(),
                            line_start,
                            line_end,
                            language: format!("{:?}", lang_kind).to_lowercase(),
                            snippet: snippet_lines.join("\n"),
                        });
                        break;
                    }
                }
            }
        }

        let formatted =
            MarkdownFormatter::format_search_results(query_str, args.scope.as_str(), &matches);

        let trace_id = args._meta.as_ref().and_then(|m| m.extract_trace_id());
        let _ = state.audit.record_entry(
            "active-session",
            trace_id.as_deref(),
            Self::NAME,
            &serde_json::to_string(&args).unwrap_or_default(),
            "SUCCESS",
            files_accessed,
            state.property_registry.load().redacted_count(),
        );

        Ok(formatted)
    }
}
