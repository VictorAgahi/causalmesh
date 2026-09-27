use mesh_core::{
    ContractNode, DocSection, GrpcTrace, ImpactFlow, ImpactMatrix, ImpactScope, RepoId,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MAX_OUTPUT_BYTES: usize = 48 * 1024; // 48 KB hard limit

/// Headroom kept out of a page's own budget for content `ToolRegistry::invoke` adds
/// around it (step 4.1 review): the bounded project-skill hint and the Git-operation
/// note, both prepended, plus slack for the header. Neither is passed into this crate,
/// so this is an upper bound on their combined size, not an exact count.
pub const NON_RESULT_RESERVE_BYTES: usize = 1536;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    pub snippet: String,
}

/// Position of one page of `smart_search` results within the full ranked set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPage {
    /// Ranked matches across all pages.
    pub total: usize,
    /// `total` is only a lower bound (a fuzzy scan stopped once the page was full).
    pub total_is_lower_bound: bool,
    /// The requested offset: rank index of this page's first candidate.
    pub offset: usize,
    /// Rank index just past the last candidate this page considered. Candidates in
    /// `offset..end` that yielded no result (unreadable, oversized) are reported as
    /// skipped, so the shown range always meets `next_offset` exactly.
    pub end: usize,
    /// `offset` to request for the next page; `None` on the last page.
    pub next_offset: Option<usize>,
}

impl SearchPage {
    /// The whole result set on one page.
    pub fn single(total: usize) -> Self {
        Self {
            total,
            total_is_lower_bound: false,
            offset: 0,
            end: total,
            next_offset: None,
        }
    }
}

pub struct MarkdownFormatter;

impl MarkdownFormatter {
    /// Formats smart search results with AST decapitated snippets and affordance truncation
    pub fn format_search_results(query: &str, scope: &str, results: &[SearchResult]) -> String {
        Self::format_search_page(query, scope, results, &SearchPage::single(results.len()))
    }

    /// One rendered result entry, exactly as [`Self::format_search_page`] emits it
    /// (`idx` is 0-based within the page).
    pub fn format_search_entry(idx: usize, r: &SearchResult) -> String {
        let snippet = r.snippet.trim();
        // Plan 4 step 4.12c: a snippet showing Markdown, a doc comment with an
        // example, or a raw string containing its own ```` ``` ```` line closed
        // this fence early, splitting the snippet across the entry boundary. The
        // fence is now longer than the longest run of backticks the snippet
        // itself contains, so no line inside it can ever close it.
        let fence = "`".repeat((longest_backtick_run(snippet) + 1).max(3));
        format!(
            "### [{}] `{}` (L{}-L{})\n{fence}{}\n{}\n{fence}\n\n",
            idx + 1,
            r.file_path,
            r.line_start,
            r.line_end,
            r.language,
            snippet
        )
    }

    /// Bytes of rendered entries ([`Self::format_search_entry`]) a paged caller may
    /// emit for `query`/`scope` while guaranteeing [`Self::format_search_page`] never
    /// falls back to its truncated output — which hides results without telling the
    /// caller where to resume. Reserves an upper bound of the header.
    pub fn search_page_entry_budget(query: &str, scope: &str) -> usize {
        const HEADER_FIXED_UPPER_BOUND: usize = 384;
        (MAX_OUTPUT_BYTES - NON_RESULT_RESERVE_BYTES)
            .saturating_sub(HEADER_FIXED_UPPER_BOUND + query.len() + scope.len())
    }

    /// [`Self::format_search_results`] for one page of a larger ranked result set:
    /// the header reports the full `total` and the range shown, and a footer tells
    /// the caller the exact `offset` to request next instead of re-running a
    /// broader query.
    pub fn format_search_page(
        query: &str,
        scope: &str,
        results: &[SearchResult],
        page: &SearchPage,
    ) -> String {
        let total = if page.total_is_lower_bound {
            format!("≥{}", page.total)
        } else {
            page.total.to_string()
        };
        let summary = if results.is_empty()
            && page.next_offset.is_none()
            && page.offset > 0
            && page.offset >= page.total
        {
            format!(
                "*No results at `offset: {}`: the result set has {total} match(es); request an offset below {}.*",
                page.offset, page.total
            )
        } else if page.offset == 0
            && page.next_offset.is_none()
            && !page.total_is_lower_bound
            && results.len() == page.total
        {
            format!("*Matches: {total} definitions found (AST-Decapitated)*")
        } else {
            let skipped = page
                .end
                .saturating_sub(page.offset)
                .saturating_sub(results.len());
            let skipped = if skipped > 0 {
                format!(", {skipped} unreadable/oversized skipped")
            } else {
                String::new()
            };
            format!(
                "*Matches: {total} definitions found (AST-Decapitated) — showing {}-{}{skipped}*",
                page.offset + 1,
                page.end
            )
        };
        let mut header =
            format!("## Search Results for `{query}` (Scope: `{scope}`)\n{summary}\n\n");

        let mut body = String::new();
        let mut scope_counts: HashMap<String, usize> = HashMap::new();

        for (idx, r) in results.iter().enumerate() {
            // Track sub-scope for truncation guidance
            let sub_scope = Self::extract_sub_scope(&r.file_path);
            *scope_counts.entry(sub_scope).or_default() += 1;

            let entry_str = Self::format_search_entry(idx, r);

            if header.len() + body.len() + entry_str.len() > MAX_OUTPUT_BYTES - 1024 {
                // Truncate with affordance guidance
                return Self::build_truncated_search_output(
                    &header,
                    &body,
                    idx,
                    results.len(),
                    query,
                    &scope_counts,
                );
            }

            body.push_str(&entry_str);
        }

        if let Some(next) = page.next_offset {
            body.push_str(&format!(
                "*More results: repeat the same call with `offset: {next}` for the next page (or narrow `scope`).*\n",
            ));
        }
        body.push_str(
            "*Tip: Use `smart_search(query: \"...\", scope: \"...\", include_body: true)` to expand an implementation.*\n",
        );

        header.push_str(&body);
        header
    }

    /// Groups a result's file path into a sub-scope for truncation guidance.
    /// Only `Normal` path components count — `RootDir`/`Prefix`/`CurDir` are
    /// skipped so an absolute path doesn't yield a bogus scope like `/Users`
    /// (the leading `/` treated as its own component) instead of the
    /// workspace-relative directory the caller actually cares about.
    fn extract_sub_scope(path: &str) -> String {
        // Borrows each component as `&str` (falling back to the lossy path
        // only for the rare non-UTF-8 one) instead of allocating a `String`
        // per component up front — only the first one or two are ever used.
        let mut components = Path::new(path).components().filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_str().unwrap_or("?")),
            _ => None,
        });
        match (components.next(), components.next()) {
            (None, _) => "root".to_string(),
            (Some(first), None) => first.to_string(),
            (Some(first), Some(second)) => format!("{first}/{second}"),
        }
    }

    fn build_truncated_search_output(
        header: &str,
        body: &str,
        displayed_count: usize,
        total_count: usize,
        query: &str,
        scope_counts: &HashMap<String, usize>,
    ) -> String {
        let mut output = String::with_capacity(MAX_OUTPUT_BYTES);
        output.push_str(header);
        output.push_str(body);

        let mut sorted_scopes: Vec<(&String, &usize)> = scope_counts.iter().collect();
        sorted_scopes.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));

        let top_scope_name = sorted_scopes
            .first()
            .map(|(s, _)| s.as_str())
            .unwrap_or("services/target");

        let guidance = format!(
            "\n[PAYLOAD TRUNCATED at 48 KB: {displayed_count} matches displayed out of {total_count} total matches]\n\n\
            💡 GUIDANCE TO PREVENT TOKEN OVERFLOW:\n\
            - Your query matched broadly across multiple sub-directories.\n\
            - Top sub-scopes detected:\n"
        );
        output.push_str(&guidance);

        for (scope_name, count) in sorted_scopes.iter().take(3) {
            output.push_str(&format!("  ├── '{scope_name}' ({count} matches)\n"));
        }

        let action = format!(
            "\n👉 ACTION REQUIRED: Repeat 'smart_search' targeting one specific sub-scope, e.g.:\n\
               smart_search(query: \"{query}\", scope: \"{top_scope_name}\")\n"
        );
        output.push_str(&action);

        output
    }

    /// `dependents` pairs each result with a human-readable label for the
    /// service/root it was crawled from (e.g. its resolved root path). A raw
    /// substring dependency match can legitimately span several unrelated
    /// services that happen to share a locally-aliased package name (every
    /// service in a polyglot monorepo vendoring its own `genproto`, for
    /// instance) — flattening those into one undifferentiated list is how a
    /// caller ends up scanning dozens of irrelevant file paths by hand to
    /// find the one real answer. Grouping by that label surfaces "matched in
    /// N services" up front instead.
    pub fn format_dependents(target: &str, dependents: &[(&ContractNode, String)]) -> String {
        let mut out = format!(
            "## In-Memory Reverse Dependency Graph for `{target}`\n*Total Dependents: {} consumer node(s) found*\n\n",
            dependents.len()
        );

        if dependents.is_empty() {
            out.push_str("No dependents found importing this symbol or package.\n");
            return out;
        }

        let mut groups: HashMap<&str, Vec<&ContractNode>> = HashMap::new();
        let mut group_order: Vec<&str> = Vec::new();
        for (node, label) in dependents {
            let bucket = groups.entry(label.as_str()).or_insert_with(|| {
                group_order.push(label.as_str());
                Vec::new()
            });
            bucket.push(node);
        }

        if group_order.len() > 1 {
            out.push_str(&format!(
                "*Matched across {} distinct services/roots — grouped below so \
                 same-named packages from unrelated services aren't flattened \
                 together.*\n\n",
                group_order.len()
            ));
        }

        let mut idx = 0;
        for label in group_order {
            let nodes = &groups[label];
            out.push_str(&format!("### {label} ({} match(es))\n\n", nodes.len()));
            for node in nodes {
                idx += 1;
                out.push_str(&format!(
                    "[{idx}] `{}` ({:?})\n- **File**: `{}:{}-{}`\n- **Package**: `{}`\n\n",
                    node.name,
                    node.kind,
                    node.file_path.display(),
                    node.line_start,
                    node.line_end,
                    node.package
                ));
            }
        }

        out
    }

    pub fn format_grpc_trace(trace: &GrpcTrace) -> String {
        let mut out = format!(
            "## End-to-End gRPC Synchronous Trace for `{}`\n\n",
            trace.target
        );

        out.push_str("### 1. Protobuf Contract Definition\n");
        if let Some(proto) = trace.proto_definition {
            let pkg = &proto.package;
            let name = &proto.name;
            out.push_str(&format!(
                "- **File**: `{}:{}`\n- **FQCN**: `{pkg}/{name}`\n- **Signature**: `{}`\n\n",
                proto.file_path.display(),
                proto.line_start,
                proto.signature.as_deref().unwrap_or("N/A")
            ));
        } else {
            out.push_str("*No formal .proto definition indexed for this target.*\n\n");
        }

        out.push_str(&format!(
            "### 2. Client Stubs ({} found)\n",
            trace.client_stubs.len()
        ));
        for (stub, confidence) in &trace.client_stubs {
            out.push_str(&format!(
                "- `{}` in `{}:{}` _(match: {})_\n",
                stub.name,
                stub.file_path.display(),
                stub.line_start,
                confidence.label()
            ));
        }
        out.push('\n');

        out.push_str(&format!(
            "### 3. Server Handlers / Controllers ({} found)\n",
            trace.server_handlers.len()
        ));
        for (handler, confidence) in &trace.server_handlers {
            out.push_str(&format!(
                "- `{}` in `{}:{}` _(match: {})_\n",
                handler.name,
                handler.file_path.display(),
                handler.line_start,
                confidence.label()
            ));
        }

        out
    }

    pub fn format_impact_flow(flow: &ImpactFlow) -> String {
        let mut out = format!(
            "## Asynchronous Causal Impact Analysis for `{}`\n\n",
            flow.target
        );

        out.push_str(&format!(
            "### 1. Upstream Event Producers ({} found)\n",
            flow.upstream_producers.len()
        ));
        for p in &flow.upstream_producers {
            out.push_str(&format!(
                "- `{}` in `{}:{}`\n",
                p.name,
                p.file_path.display(),
                p.line_start
            ));
        }
        out.push('\n');

        out.push_str(&format!(
            "### 2. Event Topics & Stream Hubs ({} found)\n",
            flow.topics.len()
        ));
        for t in &flow.topics {
            out.push_str(&format!(
                "- `{}` ({:?}) in `{}`\n",
                t.name,
                t.kind,
                t.file_path.display()
            ));
        }
        out.push('\n');

        out.push_str(&format!(
            "### 3. Downstream Consumers / Handlers ({} found)\n",
            flow.downstream_consumers.len()
        ));
        for c in &flow.downstream_consumers {
            out.push_str(&format!(
                "- `{}` in `{}:{}`\n",
                c.name,
                c.file_path.display(),
                c.line_start
            ));
        }
        out.push('\n');

        out.push_str(&format!(
            "### 4. Distributed Sagas & Post-Processors ({} found)\n",
            flow.related_sagas.len()
        ));
        for s in &flow.related_sagas {
            out.push_str(&format!(
                "- `{}` in `{}:{}`\n",
                s.name,
                s.file_path.display(),
                s.line_start
            ));
        }

        out
    }

    /// Renders one page of an [`ImpactMatrix`] (plan 4 step 4.6a) as a compact
    /// Markdown table, paged like [`Self::format_search_page`]: rows from
    /// `offset`, at most `limit` of them and never more than fit the 48 KB
    /// payload budget; the footer names the exact `offset` of the next page.
    /// `roots` are the workspace roots indexed by `repo_id`: each row shows its
    /// root (relative to the roots' common parent) and its root-relative path.
    pub fn format_impact_matrix(
        matrix: &ImpactMatrix,
        roots: &[PathBuf],
        offset: usize,
        limit: usize,
    ) -> String {
        let labels = Self::root_labels(roots);
        let label_of = |repo: RepoId| -> &str {
            labels.get(repo as usize).map(String::as_str).unwrap_or("—")
        };
        let total = matrix.rows.len();
        let external = matrix
            .rows
            .iter()
            .filter(|r| r.scope == ImpactScope::External)
            .count();

        let mut header = format!("## Impact Matrix for `{}`\n", matrix.target);
        if !matrix.contracts.is_empty() {
            let shown: Vec<String> = matrix
                .contracts
                .iter()
                .take(5)
                .map(|c| {
                    format!(
                        "`{}/{}` (`{}`)",
                        c.package,
                        c.name,
                        Self::location(c, roots)
                    )
                })
                .collect();
            let more = matrix.contracts.len().saturating_sub(5);
            let more = if more > 0 {
                format!(" … and {more} more")
            } else {
                String::new()
            };
            header.push_str(&format!("*Contract(s): {}{more}*\n", shown.join(", ")));
        }
        let owners: Vec<String> = matrix
            .owner_roots
            .iter()
            .map(|r| format!("`{}`", label_of(*r)))
            .collect();
        header.push_str(&format!(
            "*Owner root(s): {} — INTERNAL = same root as the handlers implementing the contract (else its `.proto`, or the event producers); EXTERNAL = any other root or none.*\n",
            if owners.is_empty() {
                "none (every row is EXTERNAL)".to_string()
            } else {
                owners.join(", ")
            }
        ));

        if total == 0 {
            header.push_str(
                "\n*No gRPC handler/client or async producer/topic/consumer matched this target. Pass a proto method (`ProcessPayment`, `PaymentService.ProcessPayment`), a service, or an event/topic name.*\n",
            );
            return header;
        }
        if offset >= total {
            header.push_str(&format!(
                "\n*No rows at `offset: {offset}`: the matrix has {total} row(s); request an offset below {total}.*\n"
            ));
            return header;
        }

        const FOOTER_UPPER_BOUND: usize = 160;
        const TABLE_HEAD: &str = "| # | Scope | Role | Element | Root | Location | Confidence | Via |\n|---|---|---|---|---|---|---|---|\n";
        let budget = (MAX_OUTPUT_BYTES - NON_RESULT_RESERVE_BYTES)
            .saturating_sub(header.len() + 128 + TABLE_HEAD.len() + FOOTER_UPPER_BOUND);
        let mut body = String::new();
        let mut end = offset;
        for (idx, row) in matrix
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(limit.max(1))
        {
            let via = row
                .via
                .map(|v| Self::code_cell(&v.name))
                .unwrap_or_else(|| "—".to_string());
            let line = format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                idx + 1,
                row.scope.label(),
                row.role.label(),
                Self::code_cell(&row.node.name),
                Self::code_cell(label_of(row.node.repo_id)),
                Self::code_cell(&Self::location(row.node, roots)),
                row.confidence.label(),
                via
            );
            // Always emit at least one row so a page always makes progress.
            if end > offset && body.len() + line.len() > budget {
                break;
            }
            body.push_str(&line);
            end = idx + 1;
        }

        let range = if offset == 0 && end == total {
            String::new()
        } else {
            format!(" — showing {}-{end}", offset + 1)
        };
        header.push_str(&format!(
            "*Rows: {total} ({external} EXTERNAL, {} INTERNAL){range}*\n\n",
            total - external
        ));
        header.push_str(TABLE_HEAD);
        header.push_str(&body);
        if end < total {
            header.push_str(&format!(
                "\n*More rows: repeat the same call with `offset: {end}` for the next page.*\n"
            ));
        }
        header
    }

    /// Display label of each root, indexed like `roots`: its path relative to
    /// the roots' common parent (the root's own name when that is empty).
    fn root_labels(roots: &[PathBuf]) -> Vec<String> {
        let common: Option<PathBuf> = roots.iter().fold(None, |acc, r| {
            let Some(acc) = acc else {
                return Some(r.clone());
            };
            Some(
                acc.components()
                    .zip(r.components())
                    .take_while(|(a, b)| a == b)
                    .map(|(a, _)| a)
                    .collect(),
            )
        });
        roots
            .iter()
            .map(|r| {
                let rel = common
                    .as_deref()
                    .filter(|_| roots.len() > 1)
                    .and_then(|c| r.strip_prefix(c).ok())
                    .filter(|p| !p.as_os_str().is_empty());
                match rel {
                    Some(p) => Self::slash_path(p),
                    None => r
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| r.to_string_lossy().into_owned()),
                }
            })
            .collect()
    }

    /// `path:line`, the path relative to the node's root when it has one.
    fn location(node: &ContractNode, roots: &[PathBuf]) -> String {
        match roots
            .get(node.repo_id as usize)
            .and_then(|root| node.file_path.strip_prefix(root).ok())
        {
            Some(rel) => format!("{}:{}", Self::slash_path(rel), node.line_start),
            None => format!("{}:{}", node.file_path.display(), node.line_start),
        }
    }

    /// A relative path with `/` separators on every platform, so a matrix
    /// reads (and compares) the same on Windows.
    fn slash_path(rel: &Path) -> String {
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// An inline-code table cell: `|` escaped, line breaks flattened, and a
    /// backtick fence longer than any run inside the text.
    fn code_cell(text: &str) -> String {
        let flat = text.replace(['\n', '\r'], " ").replace('|', "\\|");
        let fence = "`".repeat(longest_backtick_run(&flat) + 1);
        let pad = if flat.starts_with('`') || flat.ends_with('`') {
            " "
        } else {
            ""
        };
        format!("{fence}{pad}{flat}{pad}{fence}")
    }

    pub fn format_doc_sections(query: &str, sections: &[&DocSection]) -> String {
        let mut out = format!(
            "## Architecture Documentation Search for `{query}`\n*Matched {} conceptual section(s)*\n\n",
            sections.len()
        );

        if sections.is_empty() {
            out.push_str("No relevant architecture sections found matching your query.\n");
            return out;
        }

        for (idx, sec) in sections.iter().enumerate() {
            out.push_str(&format!(
                "### [{}] `{}` — {}\n*File: `{}:{}-{}`, Level: {}*\n\n{}\n\n---\n\n",
                idx + 1,
                sec.title,
                sec.file_path.display(),
                sec.file_path.display(),
                sec.start_line,
                sec.end_line,
                sec.level,
                sec.content.trim()
            ));
        }

        out
    }
}

/// Longest run of consecutive backticks anywhere in `s` (0 if none). Used to pick
/// a code fence long enough that no line inside the wrapped content can close it
/// early (CommonMark: a fence closes on a line of at least as many of the same
/// character and nothing else).
fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for c in s.chars() {
        if c == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impact_nodes(n: usize, name_len: usize) -> Vec<ContractNode> {
        (0..n)
            .map(|i| ContractNode {
                id: i as u32,
                name: format!("{i:04}{}", "x".repeat(name_len)).into(),
                kind: mesh_core::NodeKind::ServiceClass,
                file_path: Path::new(&format!("/ws/services/a/f{i}.go")).into(),
                line_start: i + 1,
                line_end: i + 1,
                package: "".into(),
                repo_id: (i % 2) as RepoId,
                signature: None,
                docstring: None,
            })
            .collect()
    }

    fn impact_matrix_of(nodes: &[ContractNode]) -> ImpactMatrix<'_> {
        ImpactMatrix {
            target: "Ping".into(),
            contracts: Vec::new(),
            owner_roots: vec![0],
            rows: nodes
                .iter()
                .map(|n| mesh_core::ImpactRow {
                    node: n,
                    role: mesh_core::ImpactRole::Client,
                    scope: if n.repo_id == 0 {
                        ImpactScope::Internal
                    } else {
                        ImpactScope::External
                    },
                    confidence: mesh_core::EdgeConfidence::Heuristic,
                    via: None,
                })
                .collect(),
        }
    }

    /// Plan 4 step 4.6a: pages stop at `limit` or at the byte budget, whichever
    /// comes first, and the footer's offset resumes exactly after the last row.
    #[test]
    fn impact_matrix_pages_by_limit_and_by_byte_budget() {
        let roots = vec![
            PathBuf::from("/ws/services/a"),
            PathBuf::from("/ws/services/b"),
        ];
        let nodes = impact_nodes(5, 4);
        let m = impact_matrix_of(&nodes);
        let page = MarkdownFormatter::format_impact_matrix(&m, &roots, 1, 2);
        assert!(
            page.contains("*Rows: 5 (2 EXTERNAL, 3 INTERNAL) — showing 2-3*"),
            "{page}"
        );
        // Root labels are relative to the roots' common parent; a path outside
        // the node's root is shown whole.
        assert!(
            page.contains(
                "| 2 | EXTERNAL | client | `0001xxxx` | `b` | `/ws/services/a/f1.go:2` |"
            ),
            "{page}"
        );
        assert!(
            page.contains("| 3 | INTERNAL | client | `0002xxxx` | `a` | `f2.go:3` |"),
            "{page}"
        );
        assert!(page.contains("`offset: 3`"), "{page}");
        assert!(!page.contains("| 1 |"), "{page}");

        // 2,000 rows of ~300 bytes cannot fit 48 KB: the page is cut by the
        // budget, stays under it, and resumes where it stopped.
        let nodes = impact_nodes(2000, 200);
        let m = impact_matrix_of(&nodes);
        let page = MarkdownFormatter::format_impact_matrix(&m, &roots, 0, usize::MAX);
        assert!(
            page.len() <= MAX_OUTPUT_BYTES - NON_RESULT_RESERVE_BYTES,
            "{}",
            page.len()
        );
        let shown = page
            .lines()
            .filter(|l| l.starts_with("| ") && !l.starts_with("| # "))
            .count();
        assert!(shown > 0 && shown < 2000);
        assert!(page.contains(&format!("`offset: {shown}`")), "{page}");
    }

    #[test]
    fn impact_matrix_cells_escape_pipes_and_backticks() {
        assert_eq!(MarkdownFormatter::code_cell("a|b"), "`a\\|b`");
        assert_eq!(MarkdownFormatter::code_cell("x`y"), "``x`y``");
        assert_eq!(MarkdownFormatter::code_cell("`q`"), "`` `q` ``");
        assert_eq!(MarkdownFormatter::code_cell("a\nb"), "`a b`");
        assert_eq!(
            MarkdownFormatter::root_labels(&[PathBuf::from("/ws/solo")]),
            vec!["solo".to_string()]
        );
    }

    #[test]
    fn test_format_search_results_basic() {
        let results = vec![SearchResult {
            file_path: "services/auth/src/Auth.ts".to_string(),
            line_start: 10,
            line_end: 20,
            language: "typescript".to_string(),
            snippet: "export class Auth {}".to_string(),
        }];

        let formatted = MarkdownFormatter::format_search_results("Auth", "services/auth", &results);
        assert!(formatted.contains("## Search Results for `Auth`"));
        assert!(formatted.contains("services/auth/src/Auth.ts"));
        assert!(formatted.contains("export class Auth {}"));
    }

    /// Regression (step 4.12c review): a snippet showing Markdown, a doc-comment
    /// example, or a raw string that itself contains a ```` ``` ```` line closed
    /// the entry's own fence early, splitting the snippet in two. The fence must
    /// now always be longer than the longest run of backticks inside the snippet.
    #[test]
    fn format_search_entry_fence_is_never_closed_by_the_snippet() {
        let r = SearchResult {
            file_path: "docs/example.rs".to_string(),
            line_start: 1,
            line_end: 4,
            language: "rust".to_string(),
            snippet: "/// ```\n/// let x = 1;\n/// ```\nfn f() {}".to_string(),
        };
        let entry = MarkdownFormatter::format_search_entry(0, &r);
        // Exactly two fences (open + close), never a third opened by the snippet.
        assert_eq!(entry.matches("````").count(), 2, "{entry:?}");
        assert!(
            entry.contains("/// ```\n/// let x = 1;\n/// ```"),
            "{entry:?}"
        );

        // A snippet with a longer run of backticks still round-trips: the fence
        // grows to stay longer than it.
        let r5 = SearchResult {
            snippet: "`````already fenced`````".to_string(),
            ..r
        };
        let entry5 = MarkdownFormatter::format_search_entry(0, &r5);
        assert_eq!(entry5.matches("``````").count(), 2, "{entry5:?}");

        // The common case (no backticks) still uses a plain 3-backtick fence.
        let plain = SearchResult {
            snippet: "fn f() {}".to_string(),
            ..r5
        };
        let entry_plain = MarkdownFormatter::format_search_entry(0, &plain);
        assert!(
            entry_plain.contains("```rust\nfn f() {}\n```\n"),
            "{entry_plain:?}"
        );
    }

    #[test]
    fn test_truncation_guidance() {
        let mut large_results = Vec::new();
        for i in 0..100 {
            large_results.push(SearchResult {
                file_path: format!("services/scope{}/File{}.ts", i % 5, i),
                line_start: 1,
                line_end: 50,
                language: "typescript".to_string(),
                snippet: "a".repeat(800),
            });
        }

        let formatted =
            MarkdownFormatter::format_search_results("Huge", "services", &large_results);
        assert!(formatted.contains("[PAYLOAD TRUNCATED at 48 KB"));
        assert!(formatted.contains("GUIDANCE TO PREVENT TOKEN OVERFLOW"));
        assert!(formatted.contains("ACTION REQUIRED: Repeat 'smart_search'"));
    }

    /// An absolute path used to produce a bogus scope like `/Users` (the
    /// leading `RootDir` component counted as component 0) instead of the
    /// workspace-relative directory a caller could actually act on.
    #[test]
    fn extract_sub_scope_ignores_root_component_on_absolute_paths() {
        assert_eq!(
            MarkdownFormatter::extract_sub_scope("/Users/dev/repo/services/auth/Auth.ts"),
            "Users/dev"
        );
        assert_eq!(
            MarkdownFormatter::extract_sub_scope("services/auth/Auth.ts"),
            "services/auth"
        );
        assert_eq!(MarkdownFormatter::extract_sub_scope("Auth.ts"), "Auth.ts");
    }

    /// Two sub-scopes tied on match count used to fall back to `HashMap`
    /// iteration order for tie-breaking, which is randomized per process.
    #[test]
    fn build_truncated_search_output_breaks_scope_ties_by_name() {
        let mut scope_counts = HashMap::new();
        scope_counts.insert("zzz/scope".to_string(), 3usize);
        scope_counts.insert("aaa/scope".to_string(), 3usize);
        scope_counts.insert("mmm/scope".to_string(), 3usize);

        let out =
            MarkdownFormatter::build_truncated_search_output("", "", 3, 9, "q", &scope_counts);
        let aaa_pos = out.find("aaa/scope").unwrap();
        let mmm_pos = out.find("mmm/scope").unwrap();
        let zzz_pos = out.find("zzz/scope").unwrap();
        assert!(
            aaa_pos < mmm_pos && mmm_pos < zzz_pos,
            "tied scopes must be listed in sorted-name order, not HashMap order"
        );
    }
}
