use mesh_core::{ContractNode, DocSection, GrpcTrace, ImpactFlow};
use std::collections::HashMap;
use std::path::Path;

pub const MAX_OUTPUT_BYTES: usize = 48 * 1024; // 48 KB hard limit

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
    /// Ranked matches across all pages (a lower bound when the scan stopped early).
    pub total: usize,
    /// Index of this page's first result in the ranked set.
    pub offset: usize,
    /// `offset` to request for the next page; `None` on the last page.
    pub next_offset: Option<usize>,
}

impl SearchPage {
    /// The whole result set on one page.
    pub fn single(total: usize) -> Self {
        Self {
            total,
            offset: 0,
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
        let mut header = if results.len() == page.total && page.offset == 0 {
            format!(
                "## Search Results for `{query}` (Scope: `{scope}`)\n*Matches: {} definitions found (AST-Decapitated)*\n\n",
                page.total
            )
        } else {
            let first = if results.is_empty() {
                page.offset
            } else {
                page.offset + 1
            };
            format!(
                "## Search Results for `{query}` (Scope: `{scope}`)\n*Matches: {} definitions found (AST-Decapitated) — showing {first}-{}*\n\n",
                page.total,
                page.offset + results.len()
            )
        };

        let mut body = String::new();
        let mut scope_counts: HashMap<String, usize> = HashMap::new();

        for (idx, r) in results.iter().enumerate() {
            // Track sub-scope for truncation guidance
            let sub_scope = Self::extract_sub_scope(&r.file_path);
            *scope_counts.entry(sub_scope).or_default() += 1;

            let entry_str = format!(
                "### [{}] `{}` (L{}-L{})\n```{}\n{}\n```\n\n",
                idx + 1,
                r.file_path,
                r.line_start,
                r.line_end,
                r.language,
                r.snippet.trim()
            );

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

#[cfg(test)]
mod tests {
    use super::*;

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
