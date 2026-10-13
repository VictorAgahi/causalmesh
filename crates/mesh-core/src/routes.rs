//! HTTP route matching for the graph's `HttpEndpoint` nodes.
//!
//! A route node carries its contract in `signature` (`PATCH /events/:id/state`)
//! while its `name` is the handler. Agents ask by route, in whatever spelling
//! their client or an OpenAPI document uses (`:id`, `{id}`, `<id>`, `[id]`), so
//! matching compares *segment structure*: a path parameter matches a path
//! parameter whatever its name, literals compare case-insensitively, and the
//! HTTP verb is case-insensitive. An optional global prefix (`http_prefix`) is
//! accepted with or without.

use crate::config::WorkspaceConfig;
use crate::contracts::ContractGraph;
use crate::test_paths::is_test_node;
use crate::types::{ContractNode, NodeKind, RepoId};
use std::path::{Path, PathBuf};

/// HTTP verbs a route query or an impact target may start with.
pub const HTTP_VERBS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// One path segment, parameter names erased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// `:id`, `{id}`, `<id>`, `[id]`, `${id}`, `:id?`, `[...slug]`: any value.
    Param,
    /// A literal segment, lowercased.
    Lit(String),
}

/// How well a route matched a query (lower is better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RouteRank {
    /// The whole path, with or without the configured prefix; parameters
    /// pair with parameters (names ignored).
    Exact,
    /// The whole path where a concrete value of the query (`/events/42/state`)
    /// fits a route parameter.
    Value,
    /// The query is the leading part of the route (`/events` for `/events/:id`).
    Leading,
    /// The query occurs inside the route.
    Fragment,
}

/// The route of one `HttpEndpoint` node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRoute {
    pub verb: &'static str,
    pub segs: Vec<Seg>,
}

/// A parsed `VERB /path`, bare `/path` or `VERB partial/path` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteQuery {
    pub verb: Option<&'static str>,
    pub segs: Vec<Seg>,
    /// The path was just `/` (only an exact match makes sense).
    pub root_only: bool,
    /// A verb followed by a path with a leading `/` (an impact target).
    pub verb_and_slash: bool,
}

/// Whether `seg` is `open ... close` with the opening delimiter closing exactly at
/// the last character (balanced; the inner text is free, nested delimiters included).
fn wrapped(seg: &str, open: char, close: char) -> bool {
    if !seg.starts_with(open) || !seg.ends_with(close) || seg.len() < 3 {
        return false;
    }
    let mut depth = 0usize;
    for (i, c) in seg.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return i + c.len_utf8() == seg.len();
            }
        }
    }
    false
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether the WHOLE segment is a parameter: Express `:name` with an optional
/// `(regex)` then an optional `?` / `*` / `+`; `*` or a splat `*name`;
/// `{name}` (also `{id:\d{2}}`), `<name>`, `[name]` (also `[...slug]` and
/// `[[...slug]]`), `${name}`; and `$name` (Remix / React Router file routes).
/// A mixed segment (`{name}.json`, `[id]-details`) is a literal.
///
/// Choice: `$name` is a parameter, so an OData-style literal `$metadata` is
/// treated as one too; file-route parameters are far more common in route paths.
fn is_param(seg: &str) -> bool {
    if seg == "*" {
        return true;
    }
    if let Some(rest) = seg.strip_prefix('*') {
        return is_ident(rest);
    }
    if let Some(rest) = seg.strip_prefix(':') {
        let name_len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (name, mut tail) = rest.split_at(name_len);
        if name.is_empty() {
            return false;
        }
        if tail.starts_with('(') {
            let mut depth = 0usize;
            let mut end = None;
            for (i, c) in tail.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            match end {
                Some(n) => tail = &tail[n..],
                None => return false,
            }
        }
        return matches!(tail, "" | "?" | "*" | "+");
    }
    if let Some(rest) = seg.strip_prefix('$') {
        return is_ident(rest) || wrapped(rest, '{', '}');
    }
    wrapped(seg, '{', '}') || wrapped(seg, '<', '>') || wrapped(seg, '[', ']')
}

/// Splits `path` into segments. Query string, fragment and trailing slashes are dropped.
pub fn path_segments(path: &str) -> Vec<Seg> {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    path.split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if is_param(s) {
                Seg::Param
            } else {
                Seg::Lit(s.to_lowercase())
            }
        })
        .collect()
}

/// The configured `http_prefix` as segments (empty when unset).
pub fn prefix_segments(prefix: Option<&str>) -> Vec<Seg> {
    prefix.map(path_segments).unwrap_or_default()
}

/// One resolved prefix: the text to display and its segments to match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Prefix {
    text: String,
    segs: Vec<Seg>,
}

impl Prefix {
    /// An empty text is a valid prefix: for a root it means "no prefix here",
    /// overriding the workspace-wide one.
    fn new(text: &str) -> Self {
        let text = text.trim();
        Self {
            text: text.to_string(),
            segs: path_segments(text),
        }
    }

    /// `None` for an empty text (the workspace-wide `http_prefix = ""` is unset).
    fn non_empty(text: &str) -> Option<Self> {
        let prefix = Self::new(text);
        (!prefix.text.is_empty()).then_some(prefix)
    }
}

/// The HTTP prefix of every root: the workspace-wide `http_prefix`, overridden
/// per root by `[workspace.http_prefixes]`. Indexed by `RepoId`, so a route is
/// shown (and matched) with the prefix of the root its file lives under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpPrefixes {
    workspace: Option<Prefix>,
    per_root: Vec<Option<Prefix>>,
}

/// How tightly a `[workspace.http_prefixes]` key names a root (higher wins).
/// An exact name or path beats a glob; among globs the longer key (more
/// literal text) wins, and equal keys are broken by key order so the choice
/// never depends on hash order.
fn key_specificity(key: &str, root: &Path, rel: &str) -> Option<(u8, usize)> {
    let key = key.trim().trim_start_matches("./").trim_end_matches('/');
    if key.is_empty() {
        return None;
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let abs = root.to_string_lossy().replace('\\', "/");
    if key == name || key == rel || key == abs {
        return Some((2, key.len()));
    }
    let glob = globset::Glob::new(key).ok()?.compile_matcher();
    (glob.is_match(rel) || glob.is_match(&name) || glob.is_match(&abs)).then_some((1, key.len()))
}

/// `root` relative to the workspace root, `/`-separated (`.` for the root itself);
/// the absolute path when the workspace root is unknown or does not contain it.
fn relative_to_workspace(root: &Path, workspace_root: Option<&Path>) -> String {
    let rel = workspace_root
        .and_then(|w| root.strip_prefix(w).ok())
        .unwrap_or(root);
    let joined = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

impl HttpPrefixes {
    /// One prefix for every root (the historical `http_prefix`).
    pub fn uniform(prefix: Option<&str>) -> Self {
        Self {
            workspace: prefix.and_then(Prefix::non_empty),
            per_root: Vec::new(),
        }
    }

    /// Resolves the prefix of each of `roots` (in `RepoId` order) from `ws`.
    pub fn resolve(ws: &WorkspaceConfig, roots: &[PathBuf]) -> Self {
        let workspace = ws.http_prefix.as_deref().and_then(Prefix::non_empty);
        let mut keys: Vec<(&String, &String)> = ws.http_prefixes.iter().collect();
        keys.sort();
        let per_root = roots
            .iter()
            .map(|root| {
                let rel = relative_to_workspace(root, ws.resolved_workspace_root.as_deref());
                keys.iter()
                    .filter_map(|(k, v)| key_specificity(k, root, &rel).map(|s| (s, k, v)))
                    .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
                    .map(|(_, _, v)| Prefix::new(v))
            })
            .collect();
        Self {
            workspace,
            per_root,
        }
    }

    /// Keys of `[workspace.http_prefixes]` that name none of `roots`, sorted.
    pub fn unmatched_keys(ws: &WorkspaceConfig, roots: &[PathBuf]) -> Vec<String> {
        let rels: Vec<String> = roots
            .iter()
            .map(|r| relative_to_workspace(r, ws.resolved_workspace_root.as_deref()))
            .collect();
        let mut out: Vec<String> = ws
            .http_prefixes
            .keys()
            .filter(|k| {
                !roots
                    .iter()
                    .zip(&rels)
                    .any(|(root, rel)| key_specificity(k, root, rel).is_some())
            })
            .cloned()
            .collect();
        out.sort();
        out
    }

    fn for_repo(&self, repo: RepoId) -> Option<&Prefix> {
        match self.per_root.get(repo as usize) {
            Some(Some(own)) => Some(own),
            _ => self.workspace.as_ref(),
        }
    }

    /// The prefix text to display on a route of root `repo`, if any.
    pub fn text(&self, repo: RepoId) -> Option<&str> {
        self.for_repo(repo)
            .map(|p| p.text.as_str())
            .filter(|t| !t.is_empty())
    }

    /// The prefix segments a query is matched against for a route of root `repo`.
    pub fn segs(&self, repo: RepoId) -> &[Seg] {
        self.for_repo(repo).map_or(&[], |p| p.segs.as_slice())
    }
}

/// `(verb, rest)` when `s` starts with an HTTP verb (any case) and a space.
pub fn split_verb(s: &str) -> Option<(&'static str, &str)> {
    let s = s.trim_start();
    let (head, rest) = s.split_once(char::is_whitespace)?;
    let verb = HTTP_VERBS.iter().find(|v| v.eq_ignore_ascii_case(head))?;
    Some((verb, rest.trim()))
}

/// The route a node declares: its `signature` (or, for routes named by their
/// label, its `name`) read as `VERB /path`. `None` for anything else, so
/// GraphQL / tRPC operations and nodes with no HTTP contract are ignored.
pub fn node_route(node: &ContractNode) -> Option<NodeRoute> {
    if node.kind != NodeKind::HttpEndpoint {
        return None;
    }
    let text = node
        .signature
        .as_deref()
        .filter(|s| split_verb(s).is_some())
        .unwrap_or(node.name.as_str());
    let (verb, rest) = split_verb(text)?;
    Some(NodeRoute {
        verb,
        segs: path_segments(rest),
    })
}

impl RouteQuery {
    /// `None` when `query` is not shaped like a route: a plain symbol name, or
    /// a bare verb with no path.
    pub fn parse(query: &str) -> Option<Self> {
        let query = query.trim();
        let (verb, path) = match split_verb(query) {
            Some((verb, rest)) if !rest.is_empty() => (Some(verb), rest),
            _ => (None, query),
        };
        // A route is `/path` or `VERB /path`: a verb followed by a bare word
        // (`get user`, `delete account`) is an ordinary symbol query.
        if !path.starts_with('/') {
            return None;
        }
        if path.contains(char::is_whitespace) {
            return None;
        }
        let segs = path_segments(path);
        Some(Self {
            verb,
            root_only: segs.is_empty(),
            verb_and_slash: verb.is_some() && path.starts_with('/'),
            segs,
        })
    }

    /// A strict `VERB /path` target (what `analyze_impact` resolves).
    pub fn parse_target(target: &str) -> Option<Self> {
        Self::parse(target).filter(|q| q.verb_and_slash)
    }
}

/// Whether a query segment fits a route segment. `loose` lets a concrete value
/// (`/events/42/state`) fit a route parameter; strict matching pairs parameter
/// with parameter and literal with literal only.
fn seg_fits(route: &Seg, query: &Seg, loose: bool) -> bool {
    match (route, query) {
        (Seg::Param, Seg::Param) => true,
        (Seg::Param, Seg::Lit(_)) => loose,
        (Seg::Lit(a), Seg::Lit(b)) => a == b,
        (Seg::Lit(_), Seg::Param) => false,
    }
}

fn seg_matches(route: &Seg, query: &Seg) -> bool {
    seg_fits(route, query, true)
}

fn eq_segs(route: &[Seg], query: &[Seg], loose: bool) -> bool {
    route.len() == query.len() && route.iter().zip(query).all(|(r, q)| seg_fits(r, q, loose))
}

fn starts_segs(route: &[Seg], query: &[Seg]) -> bool {
    query.len() <= route.len() && route.iter().zip(query).all(|(r, q)| seg_fits(r, q, false))
}

fn contains_segs(route: &[Seg], query: &[Seg]) -> bool {
    query.len() <= route.len()
        && (0..=route.len() - query.len()).any(|i| starts_segs(&route[i..], query))
}

/// How `route` matches `query`, given the configured `prefix` (possibly empty).
///
/// The route is compared as declared, with the prefix prepended, and with the
/// prefix removed when the route already carries it, so a query matches
/// whichever spelling the agent holds.
pub fn match_route(query: &RouteQuery, route: &NodeRoute, prefix: &[Seg]) -> Option<RouteRank> {
    if query.verb.is_some_and(|v| v != route.verb) {
        return None;
    }
    let mut variants: Vec<Vec<Seg>> = vec![route.segs.clone()];
    if !prefix.is_empty() {
        let mut full = prefix.to_vec();
        full.extend(route.segs.iter().cloned());
        variants.push(full);
        if starts_segs(&route.segs, prefix) {
            variants.push(route.segs[prefix.len()..].to_vec());
        }
    }
    if variants.iter().any(|v| eq_segs(v, &query.segs, false)) {
        return Some(RouteRank::Exact);
    }
    if variants.iter().any(|v| eq_segs(v, &query.segs, true)) {
        return Some(RouteRank::Value);
    }
    if query.root_only {
        return None;
    }
    if variants.iter().any(|v| starts_segs(v, &query.segs)) {
        return Some(RouteRank::Leading);
    }
    variants
        .iter()
        .any(|v| contains_segs(v, &query.segs))
        .then_some(RouteRank::Fragment)
}

/// A route hit: the node, its parsed route, and the match quality.
#[derive(Debug, Clone)]
pub struct RouteHit<'g> {
    pub node: &'g ContractNode,
    pub route: NodeRoute,
    pub rank: RouteRank,
}

/// The `VERB /path` text a route node declares (its `signature`, or its `name`
/// for routes named by their label). `None` for a node with no HTTP route.
pub fn route_label(node: &ContractNode) -> Option<&str> {
    node_route(node)?;
    Some(
        node.signature
            .as_deref()
            .filter(|s| split_verb(s).is_some())
            .unwrap_or(node.name.as_str()),
    )
}

/// Route declarations as the agent should read them: `PATCH /events/:id/state`,
/// with the configured prefix prepended unless the route already starts with it.
pub fn display_route(node: &ContractNode, prefix: Option<&str>) -> Option<String> {
    display_label(route_label(node)?, prefix)
}

/// [`display_route`] for an already extracted `VERB /path` label.
pub fn display_label(label: &str, prefix: Option<&str>) -> Option<String> {
    let (verb, path) = split_verb(label)?;
    let path = path.trim();
    let prefix = prefix.map(|p| p.trim().trim_end_matches('/')).unwrap_or("");
    let pre = prefix_segments(Some(prefix));
    let own = path_segments(path);
    let already =
        !pre.is_empty() && own.len() >= pre.len() && own.iter().zip(&pre).all(|(a, b)| a == b);
    if prefix.is_empty() || already {
        Some(format!("{verb} {path}"))
    } else {
        let lead = if prefix.starts_with('/') { "" } else { "/" };
        let tail = if path.starts_with('/') { "" } else { "/" };
        Some(format!("{verb} {lead}{prefix}{tail}{path}"))
    }
}

/// Every route of `graph` matching `query`, best first, then by file and line.
/// Routes in test files are left out unless `include_tests`; their count is
/// returned alongside so a caller can say so.
pub fn find_routes<'g>(
    graph: &'g ContractGraph,
    query: &RouteQuery,
    prefixes: &HttpPrefixes,
    scope: Option<&Path>,
    include_tests: bool,
    roots: &[PathBuf],
    worst_rank: RouteRank,
) -> (Vec<RouteHit<'g>>, usize) {
    let mut hits = Vec::new();
    let mut hidden_tests = 0usize;
    for node in graph.all_nodes() {
        if scope.is_some_and(|s| !node.file_path.starts_with(s)) {
            continue;
        }
        let Some(route) = node_route(node) else {
            continue;
        };
        let Some(rank) =
            match_route(query, &route, prefixes.segs(node.repo_id)).filter(|r| *r <= worst_rank)
        else {
            continue;
        };
        if !include_tests && is_test_node(node, roots) {
            hidden_tests += 1;
            continue;
        }
        hits.push(RouteHit { node, route, rank });
    }
    hits.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| a.node.file_path.cmp(&b.node.file_path))
            .then_with(|| a.node.line_start.cmp(&b.node.line_start))
            .then_with(|| a.node.id.cmp(&b.node.id))
    });
    (hits, hidden_tests)
}

/// The routes nearest to `query` when nothing matched: those sharing the most
/// leading segments (same prefix), then those with the same verb. At most `max`.
pub fn nearest_routes<'g>(
    graph: &'g ContractGraph,
    query: &RouteQuery,
    prefixes: &HttpPrefixes,
    include_tests: bool,
    roots: &[PathBuf],
    max: usize,
) -> Vec<&'g ContractNode> {
    let mut scored: Vec<(usize, bool, &'g ContractNode)> = Vec::new();
    for node in graph.all_nodes() {
        let Some(route) = node_route(node) else {
            continue;
        };
        if !include_tests && is_test_node(node, roots) {
            continue;
        }
        let same_verb = query.verb.is_none_or(|v| v == route.verb);
        let shared = [route.segs.clone(), {
            let mut full = prefixes.segs(node.repo_id).to_vec();
            full.extend(route.segs.iter().cloned());
            full
        }]
        .iter()
        .map(|segs| {
            segs.iter()
                .zip(&query.segs)
                .take_while(|(r, q)| seg_matches(r, q))
                .count()
        })
        .max()
        .unwrap_or(0);
        if shared > 0 || same_verb {
            scored.push((shared, same_verb, node));
        }
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.file_path.cmp(&b.2.file_path))
            .then_with(|| a.2.line_start.cmp(&b.2.line_start))
    });
    scored.into_iter().take(max).map(|(_, _, n)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> RouteQuery {
        RouteQuery::parse(s).expect("route query")
    }

    fn r(s: &str) -> NodeRoute {
        let (verb, rest) = split_verb(s).expect("verb");
        NodeRoute {
            verb,
            segs: path_segments(rest),
        }
    }

    #[test]
    fn parameter_spellings_and_names_are_equivalent() {
        let route = r("PATCH /events/:id/state");
        for query in [
            "PATCH /events/:id/state",
            "patch /events/{id}/state",
            "Patch /events/<id>/state",
            "PATCH /events/[id]/state",
            "PATCH /events/:eventId/state",
            "PATCH /events/:id/state/",
            "PATCH /events/:id/state?x=1",
        ] {
            assert_eq!(
                match_route(&q(query), &route, &[]),
                Some(RouteRank::Exact),
                "{query}"
            );
        }
    }

    #[test]
    fn a_concrete_value_fits_a_parameter_but_ranks_below_exact() {
        let route = r("PATCH /events/:id/state");
        assert_eq!(
            match_route(&q("PATCH /events/42/state"), &route, &[]),
            Some(RouteRank::Value)
        );
        assert!(RouteRank::Exact < RouteRank::Value);
    }

    #[test]
    fn verb_mismatch_and_literal_mismatch_do_not_match() {
        let route = r("PATCH /events/:id/state");
        assert_eq!(match_route(&q("PUT /events/:id/state"), &route, &[]), None);
        assert_eq!(
            match_route(&q("PATCH /events/:id/status"), &route, &[]),
            None
        );
        // A parameter in the query is not a wildcard over literals.
        assert_eq!(match_route(&q("PATCH /events/:id/:x"), &route, &[]), None);
    }

    #[test]
    fn fragments_rank_below_exact() {
        let route = r("GET /events/:id/state");
        assert_eq!(
            match_route(&q("/events"), &route, &[]),
            Some(RouteRank::Leading)
        );
        assert_eq!(
            match_route(&q("/state"), &route, &[]),
            Some(RouteRank::Fragment)
        );
        assert_eq!(
            match_route(&q("/events/state"), &route, &[]),
            None,
            "a literal never stands for a parameter in a fragment"
        );
        assert_eq!(
            match_route(&q("/events/:id/state"), &route, &[]),
            Some(RouteRank::Exact)
        );
        assert_eq!(
            match_route(&q("/:id/state"), &route, &[]),
            Some(RouteRank::Fragment)
        );
        assert_eq!(
            match_route(&q("GET /events/:id"), &route, &[]),
            Some(RouteRank::Leading)
        );
        assert_eq!(match_route(&q("/"), &route, &[]), None);
    }

    #[test]
    fn prefix_is_optional_in_query_and_route() {
        let prefix = prefix_segments(Some("/api/v1"));
        let route = r("GET /events/:id");
        for query in ["GET /events/:id", "GET /api/v1/events/:id"] {
            assert_eq!(
                match_route(&q(query), &route, &prefix),
                Some(RouteRank::Exact),
                "{query}"
            );
        }
        // A route that already carries the prefix still matches the bare path.
        let carried = r("GET /api/v1/events/:id");
        assert_eq!(
            match_route(&q("GET /events/:id"), &carried, &prefix),
            Some(RouteRank::Exact)
        );
        assert_eq!(
            match_route(&q("GET /events/:id"), &carried, &[]),
            Some(RouteRank::Fragment)
        );
    }

    #[test]
    fn non_route_queries_are_not_routes() {
        for s in [
            "createEvent",
            "GET",
            "UserService.GetUser",
            "",
            "get user",
            "delete account",
            "post message",
            "GET events/:id",
        ] {
            assert!(RouteQuery::parse(s).is_none(), "{s}");
        }
        assert!(RouteQuery::parse_target("/events").is_none());
        assert!(RouteQuery::parse_target("GET /events").is_some());
        assert!(RouteQuery::parse_target("GET events").is_none());
        assert!(RouteQuery::parse("get user").is_none());
    }

    #[test]
    fn only_whole_segment_parameters_are_parameters() {
        for s in [
            ":id",
            ":id?",
            ":id(\\d+)",
            "{id}",
            "{id:int}",
            "<id>",
            "[id]",
            "[...slug]",
            "${id}",
            "*",
        ] {
            assert_eq!(
                path_segments(&format!("/x/{s}")).last(),
                Some(&Seg::Param),
                "{s}"
            );
        }
        for s in [
            "{name}.json",
            "[id]-details",
            "[a][b]",
            "$",
            "{}",
            ":",
            "{a}{b}",
            "id:x",
        ] {
            assert!(
                matches!(path_segments(&format!("/x/{s}")).last(), Some(Seg::Lit(_))),
                "{s}"
            );
        }
        let json = r("GET /files/{name}.json");
        assert_eq!(match_route(&q("GET /files/{name}.xml"), &json, &[]), None);
        assert_eq!(
            match_route(&q("GET /files/{name}.json"), &json, &[]),
            Some(RouteRank::Exact)
        );
        // Mixed segments are literals: compared exactly, parameter name included.
        assert_eq!(match_route(&q("GET /files/{other}.json"), &json, &[]), None);
        assert_eq!(match_route(&q("GET /files/{name}"), &json, &[]), None);
    }

    #[test]
    fn test_nodes_are_judged_relative_to_their_root() {
        use crate::types::{CompactStr, ContractNode, NodeKind};
        let node = |path: &str| ContractNode {
            id: 0,
            name: CompactStr::new("h"),
            kind: NodeKind::HttpEndpoint,
            file_path: Path::new(path).into(),
            line_start: 1,
            line_end: 1,
            package: CompactStr::new("p"),
            repo_id: 0,
            signature: Some(CompactStr::new("GET /x")),
            docstring: None,
        };
        let roots = vec![PathBuf::from("/work/e2e/testing/gw")];
        assert!(!is_test_node(
            &node("/work/e2e/testing/gw/src/a.controller.ts"),
            &roots
        ));
        assert!(is_test_node(
            &node("/work/e2e/testing/gw/src/__test-utils__/a.controller.ts"),
            &roots
        ));
        assert!(is_test_node(
            &node("/work/e2e/testing/gw/src/a.spec.ts"),
            &roots
        ));
        // No known root: the full path is all there is.
        assert!(is_test_node(&node("/work/e2e/gw/a.ts"), &[]));
    }

    #[test]
    fn root_route_matches_only_exactly() {
        assert_eq!(
            match_route(&q("GET /"), &r("GET /"), &[]),
            Some(RouteRank::Exact)
        );
        assert_eq!(match_route(&q("GET /"), &r("GET /events"), &[]), None);
    }

    fn workspace(extra: &str, workspace_root: &str) -> WorkspaceConfig {
        let text = format!("[workspace]\nname = \"w\"\nroots = [\".\"]\n{extra}");
        let mut cfg = crate::config::Config::load_from_str(&text).expect("config");
        cfg.workspace.resolved_workspace_root = Some(PathBuf::from(workspace_root));
        cfg.workspace
    }

    fn roots() -> Vec<PathBuf> {
        [
            "/w/api-gateway",
            "/w/services/billing",
            "/w/services/users",
            "/w/npm-packages",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }

    #[test]
    fn per_root_prefixes_resolve_by_name_relative_path_and_glob() {
        let ws = workspace(
            "http_prefix = \"/pkg\"\n[workspace.http_prefixes]\n\
             \"api-gateway\" = \"/api/v1\"\n\"services/billing\" = \"/billing\"\n\"services/*\" = \"/svc\"\n",
            "/w",
        );
        let p = HttpPrefixes::resolve(&ws, &roots());
        assert_eq!(p.text(0), Some("/api/v1"), "by directory name");
        assert_eq!(p.text(1), Some("/billing"), "an exact path beats a glob");
        assert_eq!(p.text(2), Some("/svc"), "by glob");
        assert_eq!(
            p.text(3),
            Some("/pkg"),
            "unlisted keeps the workspace prefix"
        );
        assert_eq!(p.segs(0), &[Seg::Lit("api".into()), Seg::Lit("v1".into())]);
        assert_eq!(
            p.text(40),
            Some("/pkg"),
            "an unknown root id falls back too"
        );
    }

    #[test]
    fn no_prefix_anywhere_shows_none_and_keeps_http_prefix_alone_working() {
        let none = HttpPrefixes::resolve(&workspace("", "/w"), &roots());
        assert!((0..4).all(|i| none.text(i).is_none() && none.segs(i).is_empty()));
        let only = HttpPrefixes::resolve(&workspace("http_prefix = \"/api\"\n", "/w"), &roots());
        assert!((0..4).all(|i| only.text(i) == Some("/api")));
        // Only per-root: unlisted roots show nothing.
        let per = HttpPrefixes::resolve(
            &workspace("[workspace.http_prefixes]\n\"users\" = \"/u\"\n", "/w"),
            &roots(),
        );
        assert_eq!(per.text(2), Some("/u"));
        assert_eq!(per.text(0), None);
        assert_eq!(HttpPrefixes::uniform(Some("/x")).text(7), Some("/x"));
    }

    #[test]
    fn an_empty_per_root_prefix_removes_the_workspace_one_for_that_root() {
        let ws = workspace(
            "http_prefix = \"/api\"\n[workspace.http_prefixes]\n\"npm-packages\" = \"\"\n",
            "/w",
        );
        let p = HttpPrefixes::resolve(&ws, &roots());
        assert_eq!(p.text(3), None);
        assert!(p.segs(3).is_empty());
        assert_eq!(p.text(0), Some("/api"));
    }

    #[test]
    fn keys_matching_no_root_are_reported() {
        let ws = workspace(
            "[workspace.http_prefixes]\n\"api-gateway\" = \"/a\"\n\"gateway\" = \"/b\"\n\"svc/*\" = \"/c\"\n\"**/users\" = \"/d\"\n",
            "/w",
        );
        assert_eq!(
            HttpPrefixes::unmatched_keys(&ws, &roots()),
            vec!["gateway".to_string(), "svc/*".to_string()]
        );
    }

    #[test]
    fn routes_are_matched_with_the_prefix_of_their_own_root() {
        let ws = workspace(
            "[workspace.http_prefixes]\n\"api-gateway\" = \"/api\"\n",
            "/w",
        );
        let p = HttpPrefixes::resolve(&ws, &roots());
        let route = r("GET /users/:id");
        let query = q("GET /api/users/:id");
        assert_eq!(
            match_route(&query, &route, p.segs(0)),
            Some(RouteRank::Exact)
        );
        assert_eq!(match_route(&query, &route, p.segs(3)), None);
        assert_eq!(
            match_route(&q("GET /users/:id"), &route, p.segs(3)),
            Some(RouteRank::Exact)
        );
    }
}
