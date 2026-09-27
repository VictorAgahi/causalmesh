use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::{
    file_stamp, AppState, CachedSearch, CompactStr, ContractNode, FileStamp, FilesystemCrawler,
    NodeKind, PropertyRegistry, SearchCacheKey, ValidatedScope,
};
use mesh_parsers::{
    AstDecapitator, AstGuard, LanguageKind, MarkdownFormatter, SearchPage, SearchResult,
};
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

    #[serde(default)]
    #[schemars(
        with = "Option<String>",
        description = "Repository or directory to search, resolved within the configured roots. Omit it (or pass \".\" or \"*\") to search every configured root at once. DO NOT guess a parent directory of the roots: only the exact workspace root counts as global, any other path outside the roots is rejected."
    )]
    pub scope: Option<CompactStr>,

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
    #[schemars(
        description = "Upper bound on files returned in this page (1-100, default 20). A page also stops at about 8 KB of results, whichever comes first, so a page may hold fewer entries than `limit`. Results are ranked: exact symbol matches first. DO NOT raise it to see everything; page with the `offset` given in the 'More results' footer or narrow `scope` instead."
    )]
    pub limit: Option<u32>,

    #[serde(default)]
    #[schemars(
        description = "Number of ranked files to skip (default 0). Use the `offset` value given in a previous page's 'More results' footer."
    )]
    pub offset: Option<u32>,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

/// Longest snippet (in decapitated lines) returned per match.
const SNIPPET_LINES: usize = 15;
/// Lines of context shown above the matching line.
const SNIPPET_LEAD: usize = 2;
const MAX_SNIPPET_LINE_BYTES: usize = 256;
/// Files per page when the caller does not say.
const DEFAULT_LIMIT: u32 = 20;
/// Hard ceiling on `limit`: even at the snippet cap, a page stays near the 48 KB budget.
const MAX_LIMIT: u32 = 100;
/// Bytes of rendered result entries one page may carry (plan 4 step 4.13). The
/// entry count (`limit`) is only the secondary cap. Calibrated on the measured
/// sizes of a default (`limit` 20) page over the three golden corpora
/// (`scripts/bench/search_payload.py`, `docs/quality.md` 2026-09-27): every
/// page that was not cut by `limit` carried at most 6.4 KB of entries, the
/// pages `limit` did cut carried 7.7-13.3 KB, and the largest single entry was
/// 1.2 KB. 8 KiB leaves every unsaturated page unchanged, brings the heaviest
/// measured page from 15.4 KB down to 9.8 KB, and still fits six worst-case
/// entries. Header, footer and the 4.1 not-indexed note come on top of it.
const DEFAULT_PAGE_BUDGET_BYTES: usize = 8 * 1024;
/// Header and audit label of a search over every configured root.
const GLOBAL_SCOPE_LABEL: &str = "* (all configured roots)";

/// What a `scope` argument designates.
enum SearchScope {
    /// Every configured root: `scope` omitted, `"."`, `"*"`, or the exact
    /// workspace root. Decided *before* the jail, which rightly rejects the
    /// workspace root when it is only the parent of several roots — see
    /// [`SearchScope::is_global`].
    Global,
    /// A path the jail validated inside one root.
    Scoped(ValidatedScope),
}

impl SearchScope {
    fn resolve(raw: Option<&str>, state: &AppState) -> Result<Self, ToolError> {
        let workspace_root = state.config.workspace.resolved_workspace_root.as_deref();
        if Self::is_global(raw, workspace_root) {
            return Ok(Self::Global);
        }
        ValidatedScope::resolve_with_aliases(
            raw.unwrap_or_default(),
            &state.allowed_roots,
            &state.config.workspace.mount_aliases,
            workspace_root,
        )
        .map(Self::Scoped)
        .map_err(|e| (e.jsonrpc_code(), e.to_string()))
    }

    /// `None`, `"."`, `"*"`, or a path whose canonical form is exactly the
    /// resolved workspace root. Nothing else: an ancestor that merely encloses
    /// the roots (`/`, `$HOME`, a parent of the workspace) still goes through
    /// the jail and is rejected. A global search never reads outside the
    /// configured roots, so this does not widen what can be read.
    fn is_global(raw: Option<&str>, workspace_root: Option<&Path>) -> bool {
        let Some(raw) = raw else {
            return true;
        };
        if matches!(raw.trim(), "." | "./" | "*") {
            return true;
        }
        let Some(root) = workspace_root else {
            return false;
        };
        let candidate = Path::new(raw);
        let candidate = if candidate.is_relative() {
            root.join(candidate)
        } else {
            candidate.to_path_buf()
        };
        dunce::canonicalize(candidate).is_ok_and(|c| same_path(&c, root))
    }

    fn label<'a>(&self, raw: Option<&'a str>) -> &'a str {
        match self {
            Self::Global => GLOBAL_SCOPE_LABEL,
            Self::Scoped(_) => raw.unwrap_or_default(),
        }
    }
}

/// Canonical path equality with the jail's case folding (macOS/Windows).
fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (
        mesh_core::security::to_nfc_path(a),
        mesh_core::security::to_nfc_path(b),
    );
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

pub struct SmartSearchTool;

impl McpTool for SmartSearchTool {
    const NAME: &'static str = "smart_search";
    const DESCRIPTION: &'static str = "Scans scoped repositories for symbol declarations and decapitated AST signatures. Results are ranked and paginated (`limit`/`offset`); line numbers are original source coordinates. DO NOT USE to map package import hierarchies (use find_dependents).";
    type Args = SmartSearchArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn truncation_hint(args: &Self::Args, state: &AppState) -> Option<String> {
        let workspace_root = state.config.workspace.resolved_workspace_root.as_deref();
        if SearchScope::is_global(args.scope.as_deref(), workspace_root) {
            let mut roots: Vec<String> = state
                .allowed_roots
                .iter()
                .map(|r| format!("`{}`", r.display()))
                .collect();
            roots.sort();
            roots.truncate(8);
            return Some(format!(
                "Query '{}' across all configured roots returned max payload. Refine query or set `scope` to one root: {}.",
                args.query,
                roots.join(", ")
            ));
        }
        Some(format!(
            "Query '{}' in scope '{}' returned max payload. Refine query or specify a narrower subdirectory scope.",
            args.query,
            args.scope.as_deref().unwrap_or_default()
        ))
    }

    fn subject(args: &Self::Args) -> Option<&str> {
        Some(args.scope.as_deref().unwrap_or(GLOBAL_SCOPE_LABEL))
    }

    /// Index-first: the contract graph already knows every declared symbol, its
    /// file and its tree-sitter line, so the nominal path ranks files in memory and
    /// reads only the ones on the requested page — never every file that matches.
    /// The full crawl + parse of the scope is the opt-in `fuzzy` fallback. Pages
    /// are cached per snapshot generation (see [`mesh_core::SearchCache`]).
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError> {
        let raw_scope = args.scope.as_deref();
        let target = SearchScope::resolve(raw_scope, state)?;
        let scope_label = target.label(raw_scope);
        let scope_path: Option<&Path> = match &target {
            SearchScope::Global => None,
            SearchScope::Scoped(v) => Some(v.as_path()),
        };

        // Note: smart_search is a read-only discovery tool. Read access to guarded contract
        // scopes (such as proto-registry) is permitted so agents can inspect schemas and signatures.
        // Active governance (RSAH) is reserved for mutations and commit verification.

        let query = args.query.as_str();
        let matcher = QueryMatcher::new(query);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = args.offset.unwrap_or(0);
        let fuzzy = args.fuzzy.unwrap_or(false);
        let snapshot = state.snapshot();
        let generation = snapshot.generation;
        let secrets_redacted = snapshot.property_registry.redacted_count();

        let cache_key = SearchCacheKey {
            query: args.query.clone(),
            // Every global spelling renders the same page (same label).
            scope: scope_path.map(Path::to_path_buf).unwrap_or_default(),
            raw_scope: scope_label.into(),
            include_body: args.include_body,
            fuzzy,
            limit,
            offset,
        };
        if let Some(hit) = state.search_cache.get(generation, &cache_key) {
            if hit.is_fresh() {
                return Ok(ToolOutput {
                    text: hit.text.clone(),
                    files_accessed: hit.files_accessed.clone(),
                    secrets_redacted,
                });
            }
        }

        // Files inside this scope the indexer rejected (plan 4 step 4.1): named in
        // a note appended last, whose size is reserved *before* the page is laid
        // out, so results share `48 KB − note` and the note is never cut.
        let gap_note = snapshot.health.scope_note(|p| match scope_path {
            Some(scope) => p.starts_with(scope),
            None => state.allowed_roots.iter().any(|r| p.starts_with(r)),
        });
        let budget = MarkdownFormatter::search_page_entry_budget(query, scope_label)
            .saturating_sub(gap_note.as_ref().map_or(0, String::len))
            .min(DEFAULT_PAGE_BUDGET_BYTES);
        let ranked = Self::rank_indexed_files(
            snapshot.contract_graph.search_symbols(query, scope_path),
            query,
        );

        let (matches, page, file_stamps) = if !ranked.is_empty() {
            Self::collect_indexed_page(&ranked, &matcher, args.include_body, offset, limit, budget)
        } else if fuzzy {
            let files = match &target {
                SearchScope::Scoped(scope) => FilesystemCrawler::crawl_scope(
                    scope,
                    &state.config.workspace.exclude_patterns,
                    Some(8),
                ),
                SearchScope::Global => Self::crawl_all_roots(state),
            };
            let (matches, page) = Self::collect_fuzzy_page(
                &files,
                &matcher,
                args.include_body,
                offset,
                limit,
                budget,
            );
            (matches, page, Vec::new())
        } else {
            (Vec::new(), SearchPage::single(0), Vec::new())
        };
        let indexed = !ranked.is_empty();
        drop(ranked);
        drop(snapshot);

        let files_accessed: Vec<String> = matches.iter().map(|m| m.file_path.clone()).collect();
        let mut text = MarkdownFormatter::format_search_page(query, scope_label, &matches, &page);
        if let Some(note) = &gap_note {
            text.push_str(note);
        }

        // A fuzzy page reflects files on disk the index does not track, which a
        // generation bump would not invalidate — only index-backed pages are cached.
        if indexed || !fuzzy {
            state.search_cache.insert(
                generation,
                cache_key,
                CachedSearch {
                    text: text.clone(),
                    files_accessed: files_accessed.clone(),
                    file_stamps,
                },
            );
        }

        Ok(ToolOutput {
            text,
            files_accessed,
            secrets_redacted,
        })
    }
}

/// One file of the ranked result set: its best symbol's rank and the exact
/// tree-sitter line (`start_position().row + 1`) of that symbol.
struct RankedFile<'a> {
    path: &'a Path,
    rank: (u8, u8),
    anchor_line: usize,
}

/// Case-insensitive substring matcher for the query. ASCII queries (the common
/// case: identifiers) use an allocation-free byte comparison; a query with any
/// non-ASCII character falls back to Unicode `to_lowercase` so `Émetteur` still
/// finds `émetteur`.
struct QueryMatcher<'a> {
    raw: &'a str,
    unicode_lower: Option<String>,
}

impl<'a> QueryMatcher<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            unicode_lower: (!raw.is_ascii()).then(|| raw.to_lowercase()),
        }
    }

    fn matches(&self, haystack: &str) -> bool {
        match &self.unicode_lower {
            None => contains_ignore_ascii_case(haystack, self.raw),
            Some(needle) => haystack.to_lowercase().contains(needle.as_str()),
        }
    }

    /// Whether `lines` plausibly declare the matched symbol: the query itself, its
    /// `bare_names_match` normalization (last `.` segment, `_` ignored — the same
    /// rule that made the index return it), or every whitespace-separated token of
    /// a multi-word name such as `POST /orders`.
    fn mentioned_in(&self, lines: &[&str]) -> bool {
        if lines.iter().any(|l| self.matches(l)) {
            return true;
        }
        let bare = self.raw.rsplit('.').next().unwrap_or(self.raw);
        let norm = normalize_name(bare);
        if !norm.is_empty() && lines.iter().any(|l| normalize_name(l).contains(&norm)) {
            return true;
        }
        let tokens: Vec<&str> = self.raw.split_whitespace().collect();
        tokens.len() > 1
            && tokens.iter().all(|t| {
                let m = QueryMatcher::new(t);
                lines.iter().any(|l| m.matches(l))
            })
    }
}

/// Lowercased with `_` removed (see `ContractGraph::bare_names_match`).
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

impl SmartSearchTool {
    /// Fuzzy candidates of a global search: every configured root, each one
    /// re-validated by the jail and crawled like an explicit scope, merged in a
    /// deterministic order (root path, then file path). A file reachable from
    /// two nested roots is kept once.
    fn crawl_all_roots(state: &AppState) -> Vec<PathBuf> {
        let mut roots: Vec<&PathBuf> = state.allowed_roots.iter().collect();
        roots.sort();
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        for root in roots {
            let Ok(scope) = ValidatedScope::resolve(&root.to_string_lossy(), &state.allowed_roots)
            else {
                continue;
            };
            let mut part = FilesystemCrawler::crawl_scope(
                &scope,
                &state.config.workspace.exclude_patterns,
                Some(8),
            );
            part.sort();
            files.extend(part.into_iter().filter(|p| seen.insert(p.clone())));
        }
        files
    }

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

    /// Folds symbol hits into distinct files ranked by relevance rather than
    /// alphabetical path order. Each file keeps its best (lowest) rank among the
    /// symbols it declares: exact name match first, then prefix, then substring;
    /// ties broken by node kind (a declared GrpcService outranks an incidental
    /// ServiceClass), then by path. The anchor line is that best symbol's own
    /// tree-sitter line (earliest line on a rank tie), so the snippet shows the
    /// declaration that actually matched.
    fn rank_indexed_files<'a>(hits: Vec<&'a ContractNode>, query: &str) -> Vec<RankedFile<'a>> {
        let mut best: HashMap<&'a Path, ((u8, u8), usize)> = HashMap::new();
        for node in hits {
            let candidate = (Self::symbol_rank(node, query), node.line_start);
            best.entry(&*node.file_path)
                .and_modify(|cur| {
                    if candidate < *cur {
                        *cur = candidate;
                    }
                })
                .or_insert(candidate);
        }
        let mut ranked: Vec<RankedFile<'a>> = best
            .into_iter()
            .map(|(path, (rank, anchor_line))| RankedFile {
                path,
                rank,
                anchor_line,
            })
            .collect();
        ranked.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.path.cmp(b.path)));
        ranked
    }

    /// Reads only the files of the requested page. Each result's rendered size is
    /// measured before it is accepted: a result that would push the page past the
    /// formatter's entry budget is left for the next page (`next_offset` points at
    /// it), so [`MarkdownFormatter::format_search_page`] never has to truncate.
    fn collect_indexed_page(
        ranked: &[RankedFile],
        matcher: &QueryMatcher,
        include_body: bool,
        offset: u32,
        limit: u32,
        budget: usize,
    ) -> (Vec<SearchResult>, SearchPage, Vec<FileStamp>) {
        let start = (offset as usize).min(ranked.len());
        let end = start.saturating_add(limit as usize).min(ranked.len());
        let mut matches = Vec::new();
        let mut stamps = Vec::new();
        let mut bytes = 0usize;
        let mut consumed = start;
        for file in &ranked[start..end] {
            stamps.push(file_stamp(file.path));
            if let Some(result) =
                Self::search_file(file.path, matcher, include_body, Some(file.anchor_line))
            {
                let size = MarkdownFormatter::format_search_entry(matches.len(), &result).len();
                if bytes + size > budget && !matches.is_empty() {
                    stamps.pop();
                    break;
                }
                bytes += size;
                matches.push(result);
            }
            consumed += 1;
        }
        let page = SearchPage {
            total: ranked.len(),
            total_is_lower_bound: false,
            offset: offset as usize,
            end: consumed.max(offset as usize),
            next_offset: (consumed < ranked.len()).then_some(consumed),
        };
        (matches, page, stamps)
    }

    /// Full-text fallback over crawled files. Files before `offset` only get the
    /// cheap read + substring check (no decapitation); the scan stops once the page
    /// is full *and* one further real match has been seen, so "More results" is
    /// never announced for a page that would come back empty. `total` is exact when
    /// the scan reached the end, a lower bound otherwise.
    fn collect_fuzzy_page(
        files: &[PathBuf],
        matcher: &QueryMatcher,
        include_body: bool,
        offset: u32,
        limit: u32,
        budget: usize,
    ) -> (Vec<SearchResult>, SearchPage) {
        let offset = offset as usize;
        let mut matches = Vec::new();
        let mut seen = 0usize;
        let mut bytes = 0usize;
        let mut more = false;
        for path in files {
            let Some(file) = ReadFile::load(path) else {
                continue;
            };
            if !matcher.matches(&file.text) {
                continue;
            }
            seen += 1;
            if seen <= offset {
                continue;
            }
            if matches.len() >= limit as usize {
                more = true;
                break;
            }
            let Some(result) = Self::snippet(path, &file, matcher, include_body, None) else {
                seen -= 1;
                continue;
            };
            let size = MarkdownFormatter::format_search_entry(matches.len(), &result).len();
            if bytes + size > budget && !matches.is_empty() {
                more = true;
                break;
            }
            bytes += size;
            matches.push(result);
        }
        let page = SearchPage {
            total: seen,
            total_is_lower_bound: more,
            offset,
            end: offset + matches.len(),
            next_offset: more.then_some(offset + matches.len()),
        };
        (matches, page)
    }

    /// Reads one file under the AstGuard budget and extracts a snippet whose line
    /// numbers are exact original coordinates.
    ///
    /// `anchor_line` is the matched symbol's tree-sitter line (`row + 1`) from the
    /// index. It is only trusted once verified against the file as it is *now*: the
    /// anchor must be inside the file and the original lines around it must mention
    /// the query (see [`QueryMatcher::mentioned_in`]). A stale index (file edited
    /// since, or a re-parse that failed and kept last-known-good facts) or a node
    /// without a real declaration line otherwise falls back to the unanchored text
    /// search, which requires the file to contain the query at all — so a snippet is
    /// never an unrelated stretch of code labelled as the match.
    fn search_file(
        file_path: &Path,
        matcher: &QueryMatcher,
        include_body: bool,
        anchor_line: Option<usize>,
    ) -> Option<SearchResult> {
        let file = ReadFile::load(file_path)?;
        Self::snippet(file_path, &file, matcher, include_body, anchor_line)
    }

    /// An indexed anchor is usable when it lies inside the file and the original
    /// lines around it mention the query. A line-1 anchor is also accepted for a
    /// node standing for the file itself (a module named after the file stem).
    fn verified_anchor(
        file_path: &Path,
        orig_lines: &[&str],
        matcher: &QueryMatcher,
        anchor_line: Option<usize>,
    ) -> Option<usize> {
        let line = anchor_line.filter(|l| *l > 0 && *l <= orig_lines.len())?;
        let from = line.saturating_sub(1 + SNIPPET_LEAD);
        let to = orig_lines.len().min(line - 1 + SNIPPET_LINES);
        let file_is_symbol = line == 1
            && file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| matcher.mentioned_in(&[stem]));
        (file_is_symbol || matcher.mentioned_in(&orig_lines[from..to])).then_some(line)
    }

    fn snippet(
        file_path: &Path,
        file: &ReadFile,
        matcher: &QueryMatcher,
        include_body: bool,
        anchor_line: Option<usize>,
    ) -> Option<SearchResult> {
        let content_str = file.text.as_str();
        let orig_lines: Vec<&str> = content_str.lines().collect();
        let anchor_line = Self::verified_anchor(file_path, &orig_lines, matcher, anchor_line);
        if anchor_line.is_none() && !matcher.matches(content_str) {
            return None;
        }

        let path_string = file_path.to_string_lossy().into_owned();
        let lang_kind = LanguageKind::from_path(&path_string);

        if !AstGuard::should_parse_path(file_path, &file.metadata, file.text.as_bytes()) {
            // Minified / long-line file that does contain the query: return a compact
            // bounded stub informing the agent without dumping raw content.
            let line = anchor_line.unwrap_or(1);
            return Some(SearchResult {
                file_path: path_string,
                line_start: line,
                line_end: line,
                language: lang_kind.as_str().to_string(),
                snippet: format!(
                    "// [MeshMCP Note: Matched symbol '{}' in minified/oversized file (>1024b/line). Raw content bounded.]",
                    matcher.raw
                ),
            });
        }

        let decap = AstDecapitator::decapitate_auto_mapped(content_str, lang_kind, include_body);
        let decap_lines: Vec<&str> = decap.text.lines().collect();
        // A bounded stub carries no per-line map: use the original lines instead.
        let hit = if decap.bounded {
            None
        } else {
            match anchor_line {
                // First output line whose original span reaches the anchor: the line
                // holding the declaration itself, or the stripped body enclosing it.
                Some(line) => decap
                    .line_map
                    .iter()
                    .position(|&(_, last)| last as usize >= line),
                None => decap_lines.iter().position(|l| matcher.matches(l)),
            }
            .filter(|&h| h < decap_lines.len() && h < decap.line_map.len())
        };

        let (window, line_start, line_end): (Vec<&str>, usize, usize) = match hit {
            Some(hit) => {
                let start = hit.saturating_sub(SNIPPET_LEAD);
                let end = decap_lines.len().min(start + SNIPPET_LINES);
                (
                    decap_lines[start..end].to_vec(),
                    decap.line_map[start].0 as usize,
                    decap.line_map[end - 1].1 as usize,
                )
            }
            None => {
                // The query only occurs inside a stripped body (fuzzy scan), or
                // decapitation fell back to its bounded stub: show the original
                // lines around the (verified) anchor or the first mention.
                let orig_hit = match anchor_line {
                    Some(line) => line - 1,
                    None => orig_lines.iter().position(|l| matcher.matches(l))?,
                };
                let start = orig_hit.saturating_sub(SNIPPET_LEAD);
                let end = orig_lines.len().min(start + SNIPPET_LINES);
                if start >= end {
                    return None;
                }
                (orig_lines[start..end].to_vec(), start + 1, end)
            }
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
            line_start,
            line_end,
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

/// A file read under the AstGuard size budget, as valid UTF-8.
struct ReadFile {
    metadata: fs::Metadata,
    text: String,
}

impl ReadFile {
    fn load(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        // Commandment 2: size check before the read.
        if !AstGuard::within_size_budget(path, &metadata) {
            return None;
        }
        let text = String::from_utf8(fs::read(path).ok()?).ok()?;
        Some(Self { metadata, text })
    }
}

/// Allocation-free ASCII case-insensitive substring test (the old path lowered
/// the whole file into a fresh `String` per candidate).
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
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
            scope: Some(scope.into()),
            include_body: false,
            fuzzy: Some(true),
            limit: None,
            offset: None,
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

    fn py_workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        (tmp, root)
    }

    fn search(query: &str, scope: &Path, fuzzy: bool) -> SmartSearchArgs {
        SmartSearchArgs {
            query: query.into(),
            scope: Some(scope.to_string_lossy().as_ref().into()),
            include_body: false,
            fuzzy: Some(fuzzy),
            limit: None,
            offset: None,
            _meta: None,
        }
    }

    /// An indexed hit is anchored on the symbol's own tree-sitter line, not on
    /// the first textual mention of the query (a comment and a call above it).
    #[test]
    fn indexed_hit_anchors_on_declaration_line() {
        let (_tmp, root) = py_workspace();
        let mut src = String::from("# Worker helpers\ndef make():\n    return Worker()\n");
        for i in 0..30 {
            src.push_str(&format!("x_{i} = {i}\n"));
        }
        src.push_str(
            "class Worker:\n    \"\"\"A worker.\"\"\"\n    def run(self):\n        return 1\n",
        );
        let file = root.join("w.py");
        std::fs::write(&file, &src).expect("write");
        let decl_line = src
            .lines()
            .position(|l| l == "class Worker:")
            .expect("decl")
            + 1;

        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let out = SmartSearchTool::run(&search("Worker", &root, false), &state).expect("search");
        let expected = format!("(L{}-", decl_line - SNIPPET_LEAD);
        assert!(
            out.text.contains(&expected),
            "want {expected} in:\n{}",
            out.text
        );
        assert!(out.text.contains("class Worker:"), "{}", out.text);
        assert!(!out.text.contains("# Worker helpers"), "{}", out.text);
    }

    /// Fuzzy scan: the matching decapitated line is textually identical to a line
    /// inside an earlier (stripped) function body. The old re-matching reported
    /// the stripped line's number; the line map reports the real one.
    #[test]
    fn fuzzy_hit_reports_exact_line_despite_identical_earlier_text() {
        let (_tmp, root) = py_workspace();
        let src = "def setup():\n    make_target()\n\nclass Holder:\n    make_target()\n";
        let file = root.join("h.py");
        std::fs::write(&file, src).expect("write");
        let r = SmartSearchTool::search_file(&file, &QueryMatcher::new("make_target"), false, None)
            .expect("hit");
        // Hit is the class-level call on line 5; the 2-line lead starts at line 3.
        assert_eq!(r.line_start, 3, "{r:?}");
        assert_eq!(r.line_end, 5, "{r:?}");
        let first = r.snippet.lines().next().expect("line");
        assert_eq!(first, src.lines().nth(r.line_start - 1).expect("orig"));
    }

    /// Line numbers of a window spanning a stripped body cover its full extent.
    #[test]
    fn stripped_body_window_line_end_is_exact() {
        let (_tmp, root) = py_workspace();
        let src =
            "class Svc:\n    def a(self):\n        x = 1\n        y = 2\n        return x + y\n";
        let file = root.join("s.py");
        std::fs::write(&file, src).expect("write");
        let r = SmartSearchTool::search_file(&file, &QueryMatcher::new("Svc"), false, Some(1))
            .expect("hit");
        assert_eq!((r.line_start, r.line_end), (1, 5), "{r:?}");
        assert!(
            r.snippet.contains("...") && !r.snippet.contains("x = 1"),
            "{r:?}"
        );
    }

    /// `limit`/`offset` page through the ranked set; the footer names the exact
    /// next offset and disappears on the last page.
    #[test]
    fn fuzzy_pagination_pages_through_results() {
        let (_tmp, root) = py_workspace();
        for i in 0..25 {
            std::fs::write(
                root.join(format!("f{i:02}.py")),
                format!("class Thing{i}:\n    pass\n"),
            )
            .expect("write");
        }
        let state = make_state(&root, "");
        let mut a = search("Thing", &root, true);
        a.limit = Some(10);
        let p1 = SmartSearchTool::run(&a, &state).expect("p1");
        assert_eq!(p1.files_accessed.len(), 10);
        assert!(p1.text.contains("`offset: 10`"), "{}", p1.text);

        a.offset = Some(20);
        let p3 = SmartSearchTool::run(&a, &state).expect("p3");
        assert_eq!(p3.files_accessed.len(), 5);
        assert!(!p3.text.contains("More results"), "{}", p3.text);
    }

    #[test]
    fn indexed_pagination_is_disjoint_and_complete() {
        let (_tmp, root) = py_workspace();
        for i in 0..7 {
            std::fs::write(
                root.join(format!("g{i}.py")),
                format!("class Gadget{i}:\n    pass\n"),
            )
            .expect("write");
        }
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let mut a = search("Gadget", &root, false);
        a.limit = Some(3);
        let mut seen = Vec::new();
        let mut offset = 0;
        loop {
            a.offset = Some(offset);
            let page = SmartSearchTool::run(&a, &state).expect("page");
            assert!(
                page.text.contains("Matches: 7 definitions found"),
                "{}",
                page.text
            );
            seen.extend(page.files_accessed.clone());
            if !page.text.contains("More results") {
                break;
            }
            offset += 3;
        }
        let mut dedup = seen.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(seen.len(), 7);
        assert_eq!(dedup.len(), 7);
    }

    /// A page is served from cache at the same generation and recomputed after
    /// a reload bumps it — never stale.
    #[test]
    fn result_cache_invalidated_on_generation_bump() {
        let (_tmp, root) = py_workspace();
        let file = root.join("c.py");
        std::fs::write(&file, "class Cached:\n    pass\n").expect("write");
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let a = search("Cached", &root, false);

        let first = SmartSearchTool::run(&a, &state).expect("first");
        assert_eq!(state.search_cache.len(), 1);
        let again = SmartSearchTool::run(&a, &state).expect("again");
        assert_eq!(first.text, again.text);

        std::fs::write(&file, "# moved\n\n\n\nclass Cached:\n    pass\n").expect("rewrite");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let after = SmartSearchTool::run(&a, &state).expect("after");
        assert!(first.text.contains("(L1-"), "{}", first.text);
        // The declaration moved to line 5: the recomputed page anchors there.
        assert!(
            after.text.contains("(L3-"),
            "stale cache served:\n{}",
            after.text
        );
    }

    /// A relative scope resolves against the configured workspace root, not the
    /// process CWD (which for `cargo test` is the crate directory).
    #[test]
    fn relative_scope_anchors_on_workspace_root() {
        let (_tmp, root) = py_workspace();
        std::fs::create_dir_all(root.join("pkg/sub")).expect("mkdir");
        std::fs::write(root.join("pkg/sub/a.py"), "class Anchored:\n    pass\n").expect("write");
        let cfg_str = format!(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nworkspace_root = \"{}\"\nroots = [\"{}\"]\n",
            root.to_string_lossy().replace('\\', "\\\\"),
            root.to_string_lossy().replace('\\', "\\\\")
        );
        let mut config = Config::load_from_str(&cfg_str).expect("config");
        config.resolve_workspace_root(Path::new("/"));
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(config, vec![root.clone()], audit, rescan));

        let mut a = search("Anchored", Path::new("pkg/sub"), true);
        a.scope = Some("pkg/sub".into());
        let out = SmartSearchTool::run(&a, &state).expect("relative scope must resolve");
        assert_eq!(out.files_accessed.len(), 1, "{}", out.text);
    }

    /// A placeholder anchor (line 1 on a node whose real declaration is further
    /// down) is not trusted: the window must mention the query, else the snippet
    /// comes from the text search and is numbered from the query's real line.
    #[test]
    fn unverified_anchor_falls_back_to_text_search() {
        let (_tmp, root) = py_workspace();
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&format!("x_{i} = {i}\n"));
        }
        src.push_str("send(\"orders.created\")\n");
        let file = root.join("p.py");
        std::fs::write(&file, &src).expect("write");
        let m = QueryMatcher::new("orders.created");
        let r = SmartSearchTool::search_file(&file, &m, false, Some(1)).expect("hit");
        assert_eq!(r.line_start, 39, "{r:?}");
        assert!(r.snippet.contains("orders.created"), "{r:?}");
    }

    /// An anchor past EOF (stale index: the file shrank) is ignored, and a file
    /// that no longer mentions the query at all yields no result.
    #[test]
    fn stale_anchor_past_eof_is_not_trusted() {
        let (_tmp, root) = py_workspace();
        let file = root.join("s.py");
        std::fs::write(&file, "a = 1\nclass Gone2:\n    pass\n").expect("write");
        let m = QueryMatcher::new("Gone2");
        let r = SmartSearchTool::search_file(&file, &m, false, Some(500)).expect("hit");
        assert_eq!(r.line_start, 1, "{r:?}");
        assert!(r.snippet.contains("class Gone2"), "{r:?}");

        std::fs::write(&file, "a = 1\nb = 2\n").expect("rewrite");
        assert!(SmartSearchTool::search_file(&file, &m, false, Some(2)).is_none());
    }

    /// Decapitation's bounded stub (here: a >1 KB file without a grammar) carries
    /// no line map; an anchored hit shows the original lines around the anchor.
    #[test]
    fn stub_with_anchor_shows_original_lines() {
        let (_tmp, root) = py_workspace();
        let mut src = String::new();
        for i in 0..60 {
            src.push_str(&format!("prop.{i} = value-{i}\n"));
        }
        src.push_str("topic.name = orders.created\n");
        assert!(src.len() > 1024);
        let file = root.join("app.properties");
        std::fs::write(&file, &src).expect("write");
        let m = QueryMatcher::new("orders.created");
        let r = SmartSearchTool::search_file(&file, &m, false, Some(61)).expect("hit");
        assert_eq!(r.line_start, 59, "{r:?}");
        assert!(r.snippet.contains("topic.name = orders.created"), "{r:?}");
        assert!(!r.snippet.contains("MeshMCP Warning"), "{r:?}");
    }

    /// Long-line results fill the byte budget before `limit`: every page must fit
    /// without the formatter's truncation, and `next_offset` must resume exactly
    /// after the last shown result (nothing silently skipped).
    #[test]
    fn byte_budget_pages_never_truncate_or_drop() {
        let (_tmp, root) = py_workspace();
        let long = "a".repeat(240);
        for i in 0..30 {
            let mut src = format!("class Long{i:02}:\n");
            for j in 0..16 {
                src.push_str(&format!("    f{j} = \"{long}\"\n"));
            }
            std::fs::write(root.join(format!("l{i:02}.py")), src).expect("write");
        }
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let mut a = search("Long", &root, false);
        a.limit = Some(100);
        let mut seen = Vec::new();
        let mut offset = 0u32;
        let mut pages = 0;
        loop {
            a.offset = Some(offset);
            let page = SmartSearchTool::run(&a, &state).expect("page");
            pages += 1;
            assert!(!page.text.contains("PAYLOAD TRUNCATED"), "{}", page.text);
            assert!(page.text.len() <= mesh_parsers::MAX_OUTPUT_BYTES);
            assert!(!page.files_accessed.is_empty());
            seen.extend(page.files_accessed.clone());
            let next = offset + page.files_accessed.len() as u32;
            if !page.text.contains("More results") {
                break;
            }
            assert!(
                page.text.contains(&format!("`offset: {next}`")),
                "{}",
                page.text
            );
            offset = next;
        }
        assert!(pages > 1, "the budget must have split the result set");
        let mut dedup = seen.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!((seen.len(), dedup.len()), (30, 30));
    }

    /// Exactly `limit` fuzzy matches followed by non-matching files: no false
    /// "More results"; an offset past the end says so explicitly.
    #[test]
    fn fuzzy_exact_limit_has_no_false_more_results() {
        let (_tmp, root) = py_workspace();
        for i in 0..10 {
            std::fs::write(root.join(format!("a{i}.txt")), "Widget here\n").expect("write");
        }
        for i in 0..5 {
            std::fs::write(root.join(format!("z{i}.txt")), "nothing\n").expect("write");
        }
        let state = make_state(&root, "");
        let mut a = search("Widget", &root, true);
        a.limit = Some(10);
        let p1 = SmartSearchTool::run(&a, &state).expect("p1");
        assert_eq!(p1.files_accessed.len(), 10);
        assert!(!p1.text.contains("More results"), "{}", p1.text);
        assert!(
            p1.text.contains("Matches: 10 definitions found"),
            "{}",
            p1.text
        );

        a.limit = Some(4);
        let p = SmartSearchTool::run(&a, &state).expect("p");
        assert!(
            p.text.contains("Matches: ≥5 definitions found"),
            "{}",
            p.text
        );
        assert!(p.text.contains("`offset: 4`"), "{}", p.text);

        a.offset = Some(10);
        let past = SmartSearchTool::run(&a, &state).expect("past");
        assert!(past.files_accessed.is_empty());
        assert!(
            past.text.contains("No results at `offset: 10`"),
            "{}",
            past.text
        );
        assert!(!past.text.contains("More results"), "{}", past.text);
    }

    #[test]
    fn indexed_offset_past_total_is_explicit() {
        let (_tmp, root) = py_workspace();
        for i in 0..3 {
            std::fs::write(
                root.join(format!("k{i}.py")),
                format!("class Knob{i}:\n    pass\n"),
            )
            .expect("write");
        }
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let mut a = search("Knob", &root, false);
        a.offset = Some(10);
        let out = SmartSearchTool::run(&a, &state).expect("page");
        assert!(out.files_accessed.is_empty());
        assert!(
            out.text.contains("No results at `offset: 10`"),
            "{}",
            out.text
        );
        assert!(out.text.contains("3 match(es)"), "{}", out.text);
    }

    /// Two spellings of one scope share the canonical path but not the rendered
    /// header: each caller sees its own scope string.
    #[test]
    fn cache_keeps_scope_spellings_apart() {
        let (_tmp, root) = py_workspace();
        std::fs::write(root.join("c.py"), "class Spelled:\n    pass\n").expect("write");
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let plain = search("Spelled", &root, false);
        let mut dotted = plain.clone();
        dotted.scope = Some(format!("{}/.", root.to_string_lossy()).as_str().into());
        let a = SmartSearchTool::run(&plain, &state).expect("plain");
        let b = SmartSearchTool::run(&dotted, &state).expect("dotted");
        assert!(
            a.text.contains(&format!(
                "(Scope: `{}`)",
                plain.scope.as_deref().expect("scope")
            )),
            "{}",
            a.text
        );
        assert!(
            b.text.contains(&format!(
                "(Scope: `{}`)",
                dotted.scope.as_deref().expect("scope")
            )),
            "{}",
            b.text
        );
        assert_eq!(state.search_cache.len(), 2);
    }

    /// A file edited without a generation bump (debounce window, or a re-parse
    /// that failed and kept last-known-good facts) invalidates its cached page.
    #[test]
    fn cached_page_rechecks_files_on_disk() {
        let (_tmp, root) = py_workspace();
        let file = root.join("d.py");
        std::fs::write(&file, "class Drift:\n    pass\n").expect("write");
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let a = search("Drift", &root, false);
        let first = SmartSearchTool::run(&a, &state).expect("first");
        std::fs::write(&file, "class Drift:\n    renamed_attr = 1\n").expect("rewrite");
        let after = SmartSearchTool::run(&a, &state).expect("after");
        assert!(
            after.text.contains("renamed_attr"),
            "stale page served:\n{}",
            after.text
        );
        assert_ne!(first.text, after.text);
    }

    /// Non-ASCII queries fold case with Unicode rules; ASCII keeps the fast path.
    #[test]
    fn unicode_query_is_case_insensitive() {
        let m = QueryMatcher::new("ÉMETTEUR");
        assert!(m.matches("let émetteur = 1;"));
        assert!(!m.matches("let emetteur = 1;"));
        assert!(QueryMatcher::new("widget").matches("WIDGET"));
    }

    /// Writes a Python file over the 384 KB budget (so the indexer rejects it).
    fn write_oversized(path: &Path) {
        std::fs::write(path, "x = 1\n".repeat(450 * 1024 / 6 + 1)).expect("write big");
    }

    async fn invoke_search(state: &Arc<AppState>, query: &str, scope: &Path) -> String {
        let args = serde_json::json!({
            "query": query,
            "scope": scope.to_string_lossy(),
            "limit": 100,
        });
        crate::tools::ToolRegistry::invoke::<SmartSearchTool>(args, Arc::clone(state))
            .await
            .expect("no protocol fault")
            .expect("tool ok")
    }

    /// Plan 4 step 4.1: a 450 KB file inside the scope is named in a note at the
    /// end, next to the other results, and a full page plus the note stays
    /// within 48 KB with neither cut.
    #[tokio::test]
    async fn oversized_file_in_scope_is_named_after_a_full_page() {
        let (_tmp, root) = py_workspace();
        let long = "a".repeat(240);
        for i in 0..30 {
            let mut src = format!("class Long{i:02}:\n");
            for j in 0..16 {
                src.push_str(&format!("    f{j} = \"{long}\"\n"));
            }
            std::fs::write(root.join(format!("l{i:02}.py")), src).expect("write");
        }
        let big = root.join("big_long.py");
        write_oversized(&big);
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);

        let text = invoke_search(&state, "Long", &root).await;
        assert!(
            text.len() <= mesh_parsers::MAX_OUTPUT_BYTES,
            "{}",
            text.len()
        );
        assert!(text.contains("class Long00"), "other results still shown");
        assert!(
            text.contains("offset: "),
            "the page was full: {}",
            text.len()
        );
        assert!(!text.contains("PAYLOAD TRUNCATED") && !text.contains("payload truncated"));
        let note = state
            .snapshot()
            .health
            .scope_note(|p| p.starts_with(&root))
            .expect("note");
        assert!(text.ends_with(&note), "note last and intact:\n{note}");
        assert!(
            note.contains(&format!("`{}`: oversized, 450.0 KB", big.display())),
            "{note}"
        );
        assert!(note.contains("read the file directly"), "{note}");
    }

    /// A rejected file outside the query's scope never shows up in its note.
    #[tokio::test]
    async fn rejected_file_outside_scope_is_not_mentioned() {
        let (_tmp, root) = py_workspace();
        std::fs::create_dir_all(root.join("a")).expect("mkdir");
        std::fs::create_dir_all(root.join("b")).expect("mkdir");
        std::fs::write(root.join("a/svc.py"), "class Widget:\n    pass\n").expect("write");
        write_oversized(&root.join("b/huge.py"));
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        assert_eq!(state.snapshot().health.rejected.len(), 1);

        let text = invoke_search(&state, "Widget", &root.join("a")).await;
        assert!(text.contains("class Widget"), "{text}");
        assert!(
            !text.contains("huge.py") && !text.contains("not indexed"),
            "{text}"
        );
        let whole = invoke_search(&state, "Widget", &root).await;
        assert!(whole.contains("huge.py"), "{whole}");
    }

    /// The rejected list follows incremental reloads: a file that shrinks back
    /// under the budget leaves it, a newly oversized one joins it, a deleted
    /// one leaves it — and the search note follows (new generation, no stale cache).
    #[tokio::test]
    async fn rejected_list_tracks_incremental_reloads() {
        let (_tmp, root) = py_workspace();
        let big = root.join("big.py");
        write_oversized(&big);
        std::fs::write(root.join("ok.py"), "class Keep:\n    pass\n").expect("write");
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let paths = |state: &Arc<AppState>| -> Vec<PathBuf> {
            state
                .snapshot()
                .health
                .rejected
                .iter()
                .map(|r| r.path.clone())
                .collect()
        };
        assert_eq!(paths(&state), vec![big.clone()]);
        assert!(invoke_search(&state, "Keep", &root)
            .await
            .contains("big.py"));

        std::fs::write(&big, "class Big:\n    pass\n").expect("shrink");
        let other = root.join("other.py");
        write_oversized(&other);
        crate::indexer::WorkspaceIndexer::reload_paths(&state, &[big.clone(), other.clone()]);
        assert_eq!(paths(&state), vec![other.clone()]);
        let text = invoke_search(&state, "Keep", &root).await;
        assert!(
            text.contains("other.py") && !text.contains("big.py"),
            "{text}"
        );

        // An unchanged rejection is not a change: no new snapshot.
        let generation = state.snapshot().generation;
        crate::indexer::WorkspaceIndexer::reload(&state);
        assert_eq!(state.snapshot().generation, generation);

        std::fs::remove_file(&other).expect("rm");
        crate::indexer::WorkspaceIndexer::reload(&state);
        assert!(paths(&state).is_empty());
        assert!(!invoke_search(&state, "Keep", &root)
            .await
            .contains("not indexed"));
    }
    // ── Plan 4 step 4.13: global scope over several roots, default page budget ──

    /// `ws/{a,b,c}` declared as three roots, `ws` as the workspace root (the
    /// pilot's layout: the workspace root is only the roots' parent).
    fn multi_root_state(
        extra_toml: &dyn Fn(&Path) -> String,
    ) -> (tempfile::TempDir, PathBuf, Arc<AppState>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = dunce::canonicalize(tmp.path()).expect("canon").join("ws");
        let mut roots = Vec::new();
        for name in ["a", "b", "c"] {
            let root = ws.join(name);
            std::fs::create_dir_all(&root).expect("mkdir");
            std::fs::write(
                root.join(format!("svc_{name}.py")),
                format!(
                    "# needle_token\nclass Widget{}:\n    pass\n",
                    name.to_uppercase()
                ),
            )
            .expect("write");
            roots.push(root);
        }
        // Inside the workspace root but in no configured root.
        std::fs::create_dir_all(ws.join("outside")).expect("mkdir");
        std::fs::write(
            ws.join("outside/o.py"),
            "# needle_token\nclass WidgetOut:\n    pass\n",
        )
        .expect("write");
        std::fs::write(
            ws.join("top.py"),
            "# needle_token\nclass WidgetTop:\n    pass\n",
        )
        .expect("write");
        let esc = |p: &Path| p.to_string_lossy().replace('\\', "\\\\");
        let extra_toml = extra_toml(&ws);
        let cfg_str = format!(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nworkspace_root = \"{}\"\nroots = [{}]\n{extra_toml}",
            esc(&ws),
            roots
                .iter()
                .map(|r| format!("\"{}\"", esc(r)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut config = Config::load_from_str(&cfg_str).expect("config");
        config.resolve_workspace_root(Path::new("/"));
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(config, roots, audit, rescan));
        crate::indexer::WorkspaceIndexer::reload(&state);
        (tmp, ws, state)
    }

    fn with_scope(query: &str, scope: Option<&str>, fuzzy: bool) -> SmartSearchArgs {
        SmartSearchArgs {
            query: query.into(),
            scope: scope.map(Into::into),
            include_body: false,
            fuzzy: Some(fuzzy),
            limit: None,
            offset: None,
            _meta: None,
        }
    }

    fn file_names(out: &ToolOutput) -> Vec<String> {
        out.files_accessed
            .iter()
            .map(|f| {
                Path::new(f)
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn global_scope_spellings_search_every_root() {
        let (_tmp, ws, state) = multi_root_state(&|_| String::new());
        let ws_str = ws.to_string_lossy().into_owned();
        for scope in [None, Some("."), Some("*"), Some(ws_str.as_str())] {
            let out = SmartSearchTool::run(&with_scope("Widget", scope, false), &state);
            assert!(out.is_ok(), "scope {scope:?} rejected: {:?}", out.err());
            let out = out.expect("global scope");
            let mut names = file_names(&out);
            names.sort();
            assert_eq!(
                names,
                ["svc_a.py", "svc_b.py", "svc_c.py"],
                "scope {scope:?}:\n{}",
                out.text
            );
            assert!(
                out.text
                    .contains(&format!("(Scope: `{GLOBAL_SCOPE_LABEL}`)")),
                "{}",
                out.text
            );
        }
        assert_eq!(
            SmartSearchTool::subject(&with_scope("Widget", None, false)),
            Some(GLOBAL_SCOPE_LABEL)
        );
        let hint =
            SmartSearchTool::truncation_hint(&with_scope("Widget", Some("*"), false), &state)
                .expect("hint");
        assert!(hint.contains("all configured roots"), "{hint}");
        assert!(hint.contains(&ws.join("b").display().to_string()), "{hint}");
    }

    /// Without the index, the fuzzy scan walks each root in order (root, then
    /// path) and never returns a file outside the configured roots — not the
    /// workspace root's own files, not a sibling directory of the roots.
    #[test]
    fn global_fuzzy_merges_roots_in_order_and_stays_inside_them() {
        let (_tmp, _ws, state) = multi_root_state(&|_| String::new());
        let out = SmartSearchTool::run(&with_scope("needle_token", None, true), &state)
            .expect("global fuzzy");
        assert_eq!(
            file_names(&out),
            ["svc_a.py", "svc_b.py", "svc_c.py"],
            "{}",
            out.text
        );
        assert!(!out.text.contains("WidgetOut") && !out.text.contains("WidgetTop"));
        for f in &out.files_accessed {
            assert!(
                state
                    .allowed_roots
                    .iter()
                    .any(|r| Path::new(f).starts_with(r)),
                "{f} escaped the roots"
            );
        }
    }

    #[test]
    fn explicit_single_root_scope_is_unchanged() {
        let (_tmp, ws, state) = multi_root_state(&|_| String::new());
        let b = ws.join("b").to_string_lossy().into_owned();
        let out =
            SmartSearchTool::run(&with_scope("Widget", Some(&b), false), &state).expect("root b");
        assert_eq!(file_names(&out), ["svc_b.py"]);
        assert!(
            out.text.contains(&format!("(Scope: `{b}`)")),
            "{}",
            out.text
        );
        let rel = SmartSearchTool::run(&with_scope("Widget", Some("c"), false), &state)
            .expect("relative root c");
        assert_eq!(file_names(&rel), ["svc_c.py"]);
    }

    /// Only the *exact* workspace root is global: `/`, `$HOME` and a parent of
    /// the workspace still hit the jail, with several roots and with one.
    #[test]
    fn enclosing_ancestors_stay_rejected() {
        let (_tmp, ws, multi) = multi_root_state(&|_| String::new());
        let (_tmp1, single_root) = py_workspace();
        std::fs::write(single_root.join("w.py"), "class Widget:\n    pass\n").expect("write");
        let esc = single_root.to_string_lossy().replace('\\', "\\\\");
        let mut config = Config::load_from_str(&format!(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nworkspace_root = \"{esc}\"\nroots = [\"{esc}\"]\n"
        ))
        .expect("config");
        config.resolve_workspace_root(Path::new("/"));
        let single = Arc::new(AppState::new(
            config,
            vec![single_root.clone()],
            Arc::new(AuditLogger::new_in_memory().expect("audit")),
            Arc::new(BackgroundRescanEngine::new().expect("rescan")),
        ));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        for (state, parent) in [(&multi, ws.parent()), (&single, single_root.parent())] {
            let parent = parent.expect("parent").to_string_lossy().into_owned();
            for scope in ["/", home.as_str(), parent.as_str(), "..", "../.."] {
                let res = SmartSearchTool::run(&with_scope("Widget", Some(scope), true), state);
                assert!(res.is_err(), "scope {scope} must be rejected");
                let err = res.err().expect("rejected");
                assert!(
                    err.0 == -32602 || err.1.contains("not found"),
                    "scope {scope}: {err:?}"
                );
            }
        }
        // The single-root workspace's own root is global and fine.
        crate::indexer::WorkspaceIndexer::reload(&single);
        let out = SmartSearchTool::run(&with_scope("Widget", Some("."), false), &single)
            .expect("single root '.'");
        assert_eq!(file_names(&out), ["w.py"]);
    }

    #[test]
    fn mount_aliases_still_resolve_with_several_roots() {
        let (_tmp, ws, state) = multi_root_state(&|ws| {
            format!(
                "\n[workspace.mount_aliases]\n\"/workspace/b\" = \"{}\"\n",
                ws.join("b").to_string_lossy().replace('\\', "\\\\")
            )
        });
        let _ = ws;
        let out = SmartSearchTool::run(&with_scope("Widget", Some("/workspace/b"), false), &state)
            .expect("aliased root");
        assert_eq!(file_names(&out), ["svc_b.py"]);
        let unaliased =
            SmartSearchTool::run(&with_scope("Widget", Some("/workspace/a"), false), &state);
        assert!(unaliased.is_err(), "no alias for /workspace/a");
    }

    /// A default page stops at [`DEFAULT_PAGE_BUDGET_BYTES`] of entries even
    /// though `limit` (20) is not reached, and says where to resume.
    #[test]
    fn default_page_stops_at_the_byte_budget() {
        let (_tmp, root) = py_workspace();
        for i in 0..20 {
            let mut src = format!("class Heavy{i:02}:\n");
            for j in 0..12 {
                src.push_str(&format!("    field_{j} = \"{}\"\n", "v".repeat(60)));
            }
            std::fs::write(root.join(format!("h{i:02}.py")), src).expect("write");
        }
        let state = make_state(&root, "");
        crate::indexer::WorkspaceIndexer::reload(&state);
        let out = SmartSearchTool::run(&search("Heavy", &root, false), &state).expect("page");
        let shown = out.files_accessed.len();
        assert!(shown > 1 && shown < 20, "{shown} entries");
        let entries: usize = out
            .text
            .split("### [")
            .skip(1)
            .map(|e| e.len() + "### [".len())
            .sum();
        assert!(
            entries <= DEFAULT_PAGE_BUDGET_BYTES + 200,
            "{entries} bytes"
        );
        assert!(
            out.text.contains(&format!("`offset: {shown}`")),
            "{}",
            out.text
        );
    }
}
