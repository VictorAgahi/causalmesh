use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{
    AppState, CompactStr, ContractNode, FilesystemCrawler, NodeKind, PropertyRegistry,
    ValidatedScope,
};
use mesh_parsers::{AstDecapitator, AstGuard, LanguageKind, MarkdownFormatter, SearchResult};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

    fn truncation_hint(args: &Self::Args, _state: &AppState) -> Option<String> {
        Some(format!(
            "Query '{}' in scope '{}' returned max payload. Refine query or specify a narrower subdirectory scope.",
            args.query, args.scope
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.scope.as_str())
    }

    /// Index-first: the contract graph already knows every declared symbol and
    /// the file it lives in, so the nominal path reads only the files that
    /// contain a hit. The full crawl + parse of the whole scope that used to run
    /// on *every* call is now the opt-in `fuzzy` fallback.
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let validated_scope = ValidatedScope::resolve_with_aliases(
            args.scope.as_str(),
            &state.allowed_roots,
            &state.config.workspace.mount_aliases,
        )
        .map_err(|e| (e.jsonrpc_code(), e.to_string()))?;

        // Note: smart_search is a read-only discovery tool. Read access to guarded contract
        // scopes (such as proto-registry) is permitted so agents can inspect schemas and signatures.
        // Active governance (RSAH) is reserved for mutations and commit verification.

        let query = args.query.as_str();
        let snapshot = state.snapshot();

        // 1. In-memory symbol index → distinct files, ranked by relevance rather than
        // alphabetical path order. Each file's rank is the best (lowest) rank among the
        // symbol nodes it declares: exact name match first, then prefix match, then
        // substring match; ties broken by node kind (a declared GrpcService outranks an
        // incidental ServiceClass), then by path. Without this, the 48 KB output cap could
        // truncate away an exact match that happens to sort late alphabetically while an
        // incidental substring match from an earlier path survives.
        let mut best_rank: HashMap<&Path, (u8, u8)> = HashMap::new();
        for node in snapshot
            .contract_graph
            .search_symbols(query, Some(validated_scope.as_path()))
        {
            let rank = Self::symbol_rank(node, query);
            best_rank
                .entry(&*node.file_path)
                .and_modify(|r| {
                    if rank < *r {
                        *r = rank;
                    }
                })
                .or_insert(rank);
        }
        let mut ranked_files: Vec<(&Path, (u8, u8))> = best_rank.into_iter().collect();
        ranked_files.sort_by(|(path_a, rank_a), (path_b, rank_b)| {
            rank_a.cmp(rank_b).then_with(|| path_a.cmp(path_b))
        });
        let indexed_files: Vec<&Path> = ranked_files.into_iter().map(|(path, _)| path).collect();

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
    /// Ranks a symbol match for relevance ordering: (match_rank, kind_rank), both
    /// ascending (lower is better). `match_rank` distinguishes an exact symbol-name
    /// match from a prefix match from a plain substring match; `kind_rank` breaks
    /// ties in favor of protocol declarations (gRPC/HTTP/proto) over incidental
    /// structural kinds like a bare `ServiceClass`.
    fn symbol_rank(node: &ContractNode, query: &str) -> (u8, u8) {
        let name = node.name.as_str();
        let match_rank: u8 = if name.eq_ignore_ascii_case(query) {
            0
        } else if name.len() >= query.len()
            && name.as_bytes()[..query.len()].eq_ignore_ascii_case(query.as_bytes())
        {
            1
        } else {
            2
        };

        let kind_rank: u8 = match node.kind {
            NodeKind::GrpcService | NodeKind::GrpcMethod | NodeKind::HttpEndpoint => 0,
            NodeKind::ProtoMessage | NodeKind::Interface => 1,
            NodeKind::KafkaTopic | NodeKind::EventStream | NodeKind::Queue | NodeKind::Saga => 2,
            NodeKind::PostProcessor => 3,
            NodeKind::ServiceClass => 4,
        };

        (match_rank, kind_rank)
    }

    /// Reads one file under the AstGuard budget and extracts the snippet around the
    /// first line mentioning `query` mapped to original file line numbers.
    fn search_file(file_path: &Path, query: &str, include_body: bool) -> Option<SearchResult> {
        let metadata = fs::metadata(file_path).ok()?;
        // Commandment 2: size check before the read.
        if !AstGuard::within_size_budget(file_path, &metadata) {
            return None;
        }
        let content_bytes = fs::read(file_path).ok()?;
        let content_str = std::str::from_utf8(&content_bytes).ok()?;
        let query_lower = query.to_lowercase();
        let content_lower = content_str.to_lowercase();
        if !content_lower.contains(&query_lower) {
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
        let orig_lines: Vec<&str> = content_str.lines().collect();

        // Single pass over the decapitated lines: find the first hit, then take the
        // window around it. Fall back to original lines if body was decapitated.
        let decap_lines: Vec<&str> = decapitated.lines().collect();
        let (window, orig_line_start, orig_line_end) = if let Some(hit) = decap_lines
            .iter()
            .position(|l| l.to_lowercase().contains(&query_lower))
        {
            let start = hit.saturating_sub(SNIPPET_LEAD);
            let win = &decap_lines[start..decap_lines.len().min(start + SNIPPET_LINES)];

            // Map hit line to line number in original content
            let hit_line_text = decap_lines[hit].trim();
            let orig_hit = orig_lines
                .iter()
                .position(|l| l.trim() == hit_line_text)
                .or_else(|| {
                    orig_lines
                        .iter()
                        .position(|l| l.to_lowercase().contains(&query_lower))
                })
                .unwrap_or(0);
            let orig_start = (orig_hit + 1)
                .saturating_sub(hit.saturating_sub(start))
                .max(1);

            let mut orig_end = orig_hit + 1;
            if let Some(last_line) = win.last() {
                let last_trimmed = last_line.trim();
                if !last_trimmed.is_empty() {
                    if let Some(pos) = orig_lines[orig_hit..]
                        .iter()
                        .position(|l| l.trim() == last_trimmed)
                    {
                        orig_end = orig_hit + pos + 1;
                    }
                }
            }
            if orig_end < orig_start {
                orig_end = (orig_start + win.len()).min(orig_lines.len().max(1));
            }
            (win, orig_start, orig_end)
        } else {
            let orig_hit = orig_lines
                .iter()
                .position(|l| l.to_lowercase().contains(&query_lower))?;
            let start = orig_hit.saturating_sub(SNIPPET_LEAD);
            let win = &orig_lines[start..orig_lines.len().min(start + SNIPPET_LINES)];
            (win, start + 1, start + win.len())
        };

        let snippet = window
            .iter()
            .map(|l| {
                let redacted = Self::redact_sensitive_line(l);
                if redacted.len() > MAX_SNIPPET_LINE_BYTES {
                    let cut = Self::floor_char_boundary(&redacted, MAX_SNIPPET_LINE_BYTES - 3);
                    format!("{}...", &redacted[..cut])
                } else {
                    redacted
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Some(SearchResult {
            file_path: path_string,
            line_start: orig_line_start,
            line_end: orig_line_end,
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

    /// Masks a `key: value` / `key = value` / `key: "value"` line's value when
    /// `key` matches [`PropertyRegistry::is_sensitive_key`]'s patterns (e.g.
    /// `POSTGRES_PASSWORD`, `*_SECRET`, `*_TOKEN`). `smart_search` returns raw
    /// source snippets, not resolved properties, so a plaintext secret sitting
    /// right next to a matched symbol in a `.yaml`/`.env`/config file used to
    /// come back to the caller verbatim — this is the same secret-masking
    /// convention `PropertyRegistry` already applies to *resolved* config
    /// values, applied here to raw snippet lines instead.
    fn redact_sensitive_line(line: &str) -> String {
        let sep_pos = line.find([':', '=']);
        let Some(sep_pos) = sep_pos else {
            return line.to_string();
        };
        let (key_part, rest) = line.split_at(sep_pos);
        let key = key_part
            .trim()
            .trim_start_matches('-')
            .trim_matches('"')
            .trim_matches('\'');
        if key.is_empty() || key.contains(char::is_whitespace) {
            // Not a plausible `key: value` line (e.g. a URL "https://host:port"
            // or a comment) — leave it untouched rather than guess.
            return line.to_string();
        }
        thread_local! {
            static REGISTRY: PropertyRegistry = PropertyRegistry::new();
        }
        let is_sensitive = REGISTRY.with(|r| r.is_sensitive_key(key));
        if !is_sensitive {
            return line.to_string();
        }
        let sep = &rest[..1];
        format!("{key_part}{sep} {}", PropertyRegistry::REDACTED_PLACEHOLDER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{AuditLogger, BackgroundRescanEngine, Config};
    use std::sync::Arc;

    fn make_state(root: &Path, extra_toml: &str) -> Arc<AppState> {
        let cfg_str = format!(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\"{}\"]\n{extra_toml}",
            root.to_string_lossy().replace('\\', "\\\\")
        );
        let config = Config::load_from_str(&cfg_str).expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let allowed_roots = vec![root.to_path_buf()];
        Arc::new(AppState::new(config, allowed_roots, audit, rescan))
    }

    /// `smart_search` returns raw source snippets, not resolved config
    /// properties — a sensitive key sitting in a `.yaml`/`.env`-style line
    /// right next to a matched symbol used to come back to the caller in
    /// plaintext (the audit's own example: `POSTGRES_PASSWORD: accounts-pwd`).
    #[test]
    fn redact_sensitive_line_masks_secret_looking_keys() {
        assert_eq!(
            SmartSearchTool::redact_sensitive_line("POSTGRES_PASSWORD: accounts-pwd"),
            format!(
                "POSTGRES_PASSWORD: {}",
                PropertyRegistry::REDACTED_PLACEHOLDER
            )
        );
        assert_eq!(
            SmartSearchTool::redact_sensitive_line("api_token = \"sk-live-abc123\""),
            format!("api_token = {}", PropertyRegistry::REDACTED_PLACEHOLDER)
        );
    }

    #[test]
    fn redact_sensitive_line_leaves_ordinary_lines_untouched() {
        let ordinary = "  const url = \"https://api.example.com:8080/health\";";
        assert_eq!(SmartSearchTool::redact_sensitive_line(ordinary), ordinary);

        let non_secret_kv = "app.name: billing-service";
        assert_eq!(
            SmartSearchTool::redact_sensitive_line(non_secret_kv),
            non_secret_kv
        );
    }

    fn args(scope: &str) -> SmartSearchArgs {
        SmartSearchArgs {
            query: "AuthController".into(),
            scope: scope.into(),
            include_body: false,
            fuzzy: Some(true),
            _meta: None,
        }
    }

    /// `[workspace.mount_aliases]` must actually translate a container-style path
    /// (e.g. a Docker bind mount) to the real filesystem root before the sandbox
    /// jail check runs — otherwise every aliased scope is rejected outright.
    #[test]
    fn mount_alias_translates_container_path_to_real_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(root.join("a.txt"), "hello AuthController world").expect("write");

        let alias_toml = format!(
            "\n[workspace.mount_aliases]\n\"/workspace\" = \"{}\"\n",
            root.to_string_lossy().replace('\\', "\\\\")
        );
        let state = make_state(&root, &alias_toml);

        let result = SmartSearchTool::run(&args("/workspace"), &state);
        assert!(
            result.is_ok(),
            "expected /workspace alias to resolve to {}: {:?}",
            root.display(),
            result.err()
        );
    }

    /// Without a matching alias, the same container-style path must still be
    /// rejected as a sandbox escape attempt (no accidental global allow).
    #[test]
    fn without_alias_container_path_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(root.join("a.txt"), "hello AuthController world").expect("write");

        let state = make_state(&root, "");

        let result = SmartSearchTool::run(&args("/workspace"), &state);
        assert!(
            result.is_err(),
            "expected unaliased /workspace to be rejected"
        );
    }
}
