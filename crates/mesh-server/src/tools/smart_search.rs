use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{AppState, CompactStr, FilesystemCrawler, ValidatedScope};
use mesh_parsers::{AstDecapitator, AstGuard, LanguageKind, MarkdownFormatter, SearchResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

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
    #[schemars(
        description = "If true and the in-memory symbol index has no hit, falls back to a full-text scan of the scope (slower). Defaults to false."
    )]
    pub fuzzy: Option<bool>,

    #[serde(default)]
    pub _meta: Option<RequestMeta>,
}

/// Longest snippet (in decapitated lines) returned per match.
const SNIPPET_LINES: usize = 15;
/// Lines of context shown above the matching line.
const SNIPPET_LEAD: usize = 2;
const MAX_SNIPPET_LINE_BYTES: usize = 256;

pub struct SmartSearchTool;

impl McpTool for SmartSearchTool {
    const NAME: &'static str = "smart_search";
    const DESCRIPTION: &'static str = "Scans scoped repositories for symbol declarations and decapitated AST signatures. DO NOT USE to map package import hierarchies (use find_dependents).";
    type Args = SmartSearchArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    /// Index-first: the contract graph already knows every declared symbol and
    /// the file it lives in, so the nominal path reads only the files that
    /// contain a hit. The full crawl + parse of the whole scope that used to run
    /// on *every* call is now the opt-in `fuzzy` fallback.
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let validated_scope = ValidatedScope::resolve(args.scope.as_str(), &state.allowed_roots)
            .map_err(|e| (e.jsonrpc_code(), e.to_string()))?;

        // Note: smart_search is a read-only discovery tool. Read access to guarded contract
        // scopes (such as proto-registry) is permitted so agents can inspect schemas and signatures.
        // Active governance (RSAH) is reserved for mutations and commit verification.

        let query = args.query.as_str();
        let snapshot = state.snapshot();

        // 1. In-memory symbol index → distinct files, in stable order.
        let indexed_files: BTreeSet<&Path> = snapshot
            .contract_graph
            .search_symbols(query, Some(validated_scope.as_path()))
            .into_iter()
            .map(|n| &*n.file_path)
            .collect();

        let candidate_files: Vec<PathBuf> = if !indexed_files.is_empty() {
            indexed_files.into_iter().map(Path::to_path_buf).collect()
        } else if args.fuzzy.unwrap_or(false) {
            FilesystemCrawler::crawl_scope(
                &validated_scope,
                &state.config.workspace.exclude_patterns,
                Some(8),
            )
        } else {
            Vec::new()
        };
        drop(snapshot);

        let mut matches = Vec::new();
        let mut files_accessed = Vec::new();

        for file_path in candidate_files {
            let Some(result) = Self::search_file(&file_path, query, args.include_body) else {
                continue;
            };
            files_accessed.push(result.file_path.clone());
            matches.push(result);
        }

        let formatted =
            MarkdownFormatter::format_search_results(query, args.scope.as_str(), &matches);

        Ok(ToolOutput {
            text: formatted,
            files_accessed,
            secrets_redacted: state.snapshot().property_registry.redacted_count(),
        })
    }
}

impl SmartSearchTool {
    /// Reads one file under the AstGuard budget and extracts the snippet around the
    /// first line mentioning `query` in its decapitated form.
    fn search_file(file_path: &Path, query: &str, include_body: bool) -> Option<SearchResult> {
        let metadata = fs::metadata(file_path).ok()?;
        // Commandment 2: size check before the read.
        if !AstGuard::within_size_budget(file_path, &metadata) {
            return None;
        }
        let content_bytes = fs::read(file_path).ok()?;
        let content_str = std::str::from_utf8(&content_bytes).ok()?;
        if !content_str.contains(query) {
            return None;
        }

        let path_string = file_path.to_string_lossy().into_owned();
        let lang_kind = LanguageKind::from_path(&path_string);

        if !AstGuard::should_parse_path(file_path, &metadata, &content_bytes) {
            // Minified / long-line file that does contain the query: return a compact
            // bounded stub informing the agent without dumping raw content.
            return Some(SearchResult {
                file_path: path_string,
                line_start: 1,
                line_end: 1,
                language: lang_kind.as_str().to_string(),
                snippet: format!(
                    "// [MeshMCP Note: Matched symbol '{query}' in minified/oversized file (>1024b/line). Raw content bounded.]"
                ),
            });
        }

        let decapitated = AstDecapitator::decapitate_auto(content_str, lang_kind, include_body);

        // Single pass over the decapitated lines: find the first hit, then take the
        // window around it (was a second `.lines().skip(n)` walk from the start).
        let lines: Vec<&str> = decapitated.lines().collect();
        let hit = lines.iter().position(|l| l.contains(query))?;
        let start = hit.saturating_sub(SNIPPET_LEAD);
        let window = &lines[start..lines.len().min(start + SNIPPET_LINES)];

        let snippet = window
            .iter()
            .map(|l| {
                if l.len() > MAX_SNIPPET_LINE_BYTES {
                    let cut = Self::floor_char_boundary(l, MAX_SNIPPET_LINE_BYTES - 3);
                    format!("{}...", &l[..cut])
                } else {
                    (*l).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Some(SearchResult {
            file_path: path_string,
            line_start: start + 1,
            line_end: start + window.len(),
            language: lang_kind.as_str().to_string(),
            snippet,
        })
    }

    /// `&l[..253]` on a multi-byte line panics; back off to a char boundary.
    fn floor_char_boundary(s: &str, idx: usize) -> usize {
        let mut i = idx.min(s.len());
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}
