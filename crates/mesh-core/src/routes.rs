//! HTTP route matching for the graph's `HttpEndpoint` nodes.
//!
//! A route node carries its contract in `signature` (`PATCH /events/:id/state`)
//! while its `name` is the handler. Agents ask by route, in whatever spelling
//! their client or an OpenAPI document uses (`:id`, `{id}`, `<id>`, `[id]`), so
//! matching compares *segment structure*: a path parameter matches a path
//! parameter whatever its name, literals compare case-insensitively, and the
//! HTTP verb is case-insensitive. An optional global prefix (`http_prefix`) is
//! accepted with or without.

use crate::contracts::ContractGraph;
use crate::types::{ContractNode, NodeKind};
use std::path::Path;

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

fn is_param(seg: &str) -> bool {
    let first = seg.chars().next();
    matches!(first, Some(':' | '{' | '<' | '[' | '$')) || seg == "*"
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
        if verb.is_none() && !path.starts_with('/') {
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
    prefix: &[Seg],
    scope: Option<&Path>,
    include_tests: bool,
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
        let Some(rank) = match_route(query, &route, prefix) else {
            continue;
        };
        if !include_tests && crate::is_test_path(&node.file_path) {
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
    prefix: &[Seg],
    include_tests: bool,
    max: usize,
) -> Vec<&'g ContractNode> {
    let mut scored: Vec<(usize, bool, &'g ContractNode)> = Vec::new();
    for node in graph.all_nodes() {
        let Some(route) = node_route(node) else {
            continue;
        };
        if !include_tests && crate::is_test_path(&node.file_path) {
            continue;
        }
        let same_verb = query.verb.is_none_or(|v| v == route.verb);
        let shared = [route.segs.clone(), {
            let mut full = prefix.to_vec();
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
            match_route(&q("GET events/:id"), &route, &[]),
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
        for s in ["createEvent", "GET", "UserService.GetUser", ""] {
            assert!(RouteQuery::parse(s).is_none(), "{s}");
        }
        assert!(RouteQuery::parse_target("/events").is_none());
        assert!(RouteQuery::parse_target("GET /events").is_some());
        assert!(RouteQuery::parse_target("GET events").is_none());
    }

    #[test]
    fn root_route_matches_only_exactly() {
        assert_eq!(
            match_route(&q("GET /"), &r("GET /"), &[]),
            Some(RouteRank::Exact)
        );
        assert_eq!(match_route(&q("GET /"), &r("GET /events"), &[]), None);
    }
}
