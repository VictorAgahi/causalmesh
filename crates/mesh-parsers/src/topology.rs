//! Per-service aggregated topology for `visualize_mesh`.
//!
//! The flat contract graph is the wrong unit for an MCP answer: at a few
//! thousand nodes its Mermaid/JSON/HTML rendering already exceeds the 48 KB
//! payload cap and gets cut mid-document (invalid HTML/JSON, a Mermaid graph
//! missing half its edges). This view folds every contract into its *service*
//! — the workspace root it was scanned from when there are several roots, its
//! package otherwise — and every cross-service edge into one weighted link per
//! `(from, to, kind)`. Event-bus topics stay first-class nodes: they are the
//! causal glue between services.
//!
//! Size is bounded the same way `MarkdownFormatter`'s search truncation is:
//! the best-connected groups are shown, the rest fold into one
//! "other services" / "other topics" node, and the footer names the largest
//! hidden groups and the exact call to zoom into one of them.

use crate::graph::{WebEdge, WebGraphPayload, WebNode};
use mesh_core::{ContractGraph, ContractNode, EdgeConfidence, EdgeKind, NodeId, NodeKind, RepoId};
use std::collections::{BTreeMap, HashMap};

/// What a topology node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupKind {
    Service,
    Topic,
    /// Folded remainder ("other services" / "other topics").
    Other,
    /// One contract of the focused service (zoom view only).
    Contract,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub kind: GroupKind,
    /// Contracts folded into this node (1 for a `Contract`).
    pub contracts: usize,
    /// Contract count per `NodeKind` label, for the node caption.
    pub by_kind: BTreeMap<&'static str, usize>,
    /// Edges between two contracts of this same group (not drawn).
    pub internal_edges: usize,
    /// Groups folded into an `Other` node.
    pub folded: usize,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    pub count: usize,
    /// The weakest confidence among the folded edges.
    pub weakest: EdgeConfidence,
}

#[derive(Debug, Clone)]
pub struct Topology {
    pub groups: Vec<Group>,
    pub links: Vec<Link>,
    /// Services + topics before folding.
    pub total_groups: usize,
    pub total_contracts: usize,
    pub total_edges: usize,
    /// Every hidden group, `(name, contracts, kind)`, biggest first.
    pub hidden: Vec<(String, usize, GroupKind)>,
    /// What a "service" is here: `"root"` (one per workspace root) or `"package"`.
    pub grouping: &'static str,
    /// The zoomed-in service, when one was requested and found.
    pub focus: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TopologyOptions {
    /// Services/topics drawn before the rest fold into "other".
    pub max_groups: usize,
    /// Contracts of the focused service drawn individually.
    pub max_focus_contracts: usize,
    /// Zoom into one service (exact group name, case-insensitive).
    pub focus: Option<String>,
}

impl Default for TopologyOptions {
    fn default() -> Self {
        Self {
            max_groups: 40,
            max_focus_contracts: 40,
            focus: None,
        }
    }
}

fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::GrpcService => "gRPC service",
        NodeKind::GrpcMethod => "gRPC method",
        NodeKind::HttpEndpoint => "HTTP",
        NodeKind::ProtoMessage => "message",
        NodeKind::Interface => "interface",
        NodeKind::KafkaTopic | NodeKind::EventStream | NodeKind::Queue => "topic",
        NodeKind::Saga => "saga",
        NodeKind::PostProcessor => "post-processor",
        NodeKind::ServiceClass => "class",
    }
}

fn edge_label(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Produces => "Produces",
        EdgeKind::Consumes => "Consumes",
        EdgeKind::CallsRpc => "CallsRpc",
        EdgeKind::Implements => "Implements",
        EdgeKind::Imports => "Imports",
        EdgeKind::DispatchesTo => "Dispatches",
    }
}

fn confidence_rank(c: EdgeConfidence) -> u8 {
    match c {
        EdgeConfidence::Exact => 0,
        EdgeConfidence::Heuristic => 1,
        EdgeConfidence::Ambiguous => 2,
    }
}

fn is_topic_hub(node: &ContractNode) -> bool {
    node.repo_id == RepoId::MAX
        && matches!(
            node.kind,
            NodeKind::EventStream | NodeKind::KafkaTopic | NodeKind::Queue
        )
}

/// Group of one contract: `(name, kind)`, borrowed from the node or the root
/// names — building the view never allocates per contract.
fn group_of<'a>(
    node: &'a ContractNode,
    repo_names: &'a [String],
    by_root: bool,
) -> (&'a str, GroupKind) {
    if is_topic_hub(node) {
        return (node.name.as_str(), GroupKind::Topic);
    }
    if by_root {
        if let Some(repo) = repo_names.get(node.repo_id as usize) {
            return (repo.as_str(), GroupKind::Service);
        }
    }
    if node.package.is_empty() {
        ("shared", GroupKind::Service)
    } else {
        (node.package.as_str(), GroupKind::Service)
    }
}

/// One cross-group edge bundle before folding: `(from, to, kind)` → count and
/// weakest confidence rank.
#[derive(Debug, Clone)]
struct AggLink {
    from: u32,
    to: u32,
    kind: EdgeKind,
    count: usize,
    weakest: u8,
}

/// The unfolded per-service view: everything `visualize_mesh` needs that does
/// not depend on how many groups are drawn. It is the O(contracts + edges)
/// part, so it is built once per call; [`Aggregate::select`] is then cheap
/// (O(groups + links)) and is what the renderer's shrink-to-fit loop repeats.
#[derive(Debug, Clone)]
pub struct Aggregate {
    groups: Vec<Group>,
    /// Node id of a zoom `Contract` group (its name is not unique), 0 otherwise.
    /// With `(kind, name)` it makes the ranking a total, deterministic order.
    node_ids: Vec<NodeId>,
    links: Vec<AggLink>,
    /// Cross-group edges touching each group.
    degree: Vec<usize>,
    /// Zoom only: edges between each group and the focused service's contracts.
    focus_weight: Vec<usize>,
    /// Group indices, best-connected first.
    ranked: Vec<u32>,
    total_contracts: usize,
    total_edges: usize,
    by_root: bool,
    focus: Option<String>,
}

impl Aggregate {
    /// Folds the graph into services and topics. `focus` names a service to
    /// explode into its own contracts: an exact match wins, otherwise the
    /// case-insensitive one (the first in byte order if several differ only by
    /// case). An unknown `focus` leaves `focus()` at `None`.
    pub fn build(graph: &ContractGraph, repo_names: &[String], focus: Option<&str>) -> Self {
        // One root → grouping by root would draw a single box; use packages.
        // Stops at the second distinct root instead of collecting every id.
        let by_root = {
            let mut first: Option<RepoId> = None;
            graph
                .all_nodes()
                .filter(|n| !is_topic_hub(n) && (n.repo_id as usize) < repo_names.len())
                .any(|n| *first.get_or_insert(n.repo_id) != n.repo_id)
        };

        let focus: Option<String> = focus.and_then(|f| {
            let mut best: Option<&str> = None;
            for node in graph.all_nodes() {
                let (name, kind) = group_of(node, repo_names, by_root);
                if kind != GroupKind::Service {
                    continue;
                }
                if name == f {
                    return Some(name.to_string());
                }
                if name.eq_ignore_ascii_case(f) && best.is_none_or(|b| name < b) {
                    best = Some(name);
                }
            }
            best.map(str::to_string)
        });

        // Group indices are assigned in node-id order (the graph's BTreeMap),
        // so every index below is deterministic.
        let mut groups: Vec<Group> = Vec::new();
        let mut node_ids: Vec<NodeId> = Vec::new();
        let mut interned: HashMap<(GroupKind, &str), u32> = HashMap::new();
        let mut node_group: HashMap<NodeId, u32> = HashMap::with_capacity(graph.node_count());
        for node in graph.all_nodes() {
            let (name, kind) = group_of(node, repo_names, by_root);
            let gi = if kind == GroupKind::Service && focus.as_deref() == Some(name) {
                groups.push(Group {
                    name: format!("{} [{}]", node.name, kind_label(node.kind)),
                    kind: GroupKind::Contract,
                    contracts: 1,
                    by_kind: BTreeMap::new(),
                    internal_edges: 0,
                    folded: 0,
                });
                node_ids.push(node.id);
                (groups.len() - 1) as u32
            } else {
                let gi = *interned.entry((kind, name)).or_insert_with(|| {
                    groups.push(Group {
                        name: name.to_string(),
                        kind,
                        contracts: 0,
                        by_kind: BTreeMap::new(),
                        internal_edges: 0,
                        folded: 0,
                    });
                    node_ids.push(0);
                    (groups.len() - 1) as u32
                });
                let g = &mut groups[gi as usize];
                g.contracts += 1;
                *g.by_kind.entry(kind_label(node.kind)).or_default() += 1;
                gi
            };
            node_group.insert(node.id, gi);
        }

        // Cross-group edge weights per (from, to, kind).
        let mut weights: HashMap<(u32, u32, u8), AggLink> = HashMap::new();
        let mut degree = vec![0usize; groups.len()];
        let edges = graph.all_edges();
        for e in edges {
            let (Some(&from), Some(&to)) = (node_group.get(&e.from), node_group.get(&e.to)) else {
                continue;
            };
            if from == to {
                groups[from as usize].internal_edges += 1;
                continue;
            }
            let w = weights
                .entry((from, to, e.kind as u8))
                .or_insert_with(|| AggLink {
                    from,
                    to,
                    kind: e.kind,
                    count: 0,
                    weakest: 0,
                });
            w.count += 1;
            w.weakest = w.weakest.max(confidence_rank(e.confidence));
            degree[from as usize] += 1;
            degree[to as usize] += 1;
        }
        let mut links: Vec<AggLink> = weights.into_values().collect();
        links.sort_unstable_by_key(|l| (l.from, l.to, l.kind as u8));

        let mut focus_weight = vec![0usize; groups.len()];
        if focus.is_some() {
            for l in &links {
                let (a, b) = (l.from as usize, l.to as usize);
                match (groups[a].kind, groups[b].kind) {
                    (GroupKind::Contract, GroupKind::Contract) => {}
                    (GroupKind::Contract, _) => focus_weight[b] += l.count,
                    (_, GroupKind::Contract) => focus_weight[a] += l.count,
                    _ => {}
                }
            }
        }

        let mut agg = Self {
            groups,
            node_ids,
            links,
            degree,
            focus_weight,
            ranked: Vec::new(),
            total_contracts: graph.node_count(),
            total_edges: edges.len(),
            by_root,
            focus,
        };
        let mut ranked: Vec<u32> = (0..agg.groups.len() as u32).collect();
        ranked.sort_unstable_by(|&a, &b| agg.rank_cmp(a, b));
        agg.ranked = ranked;
        agg
    }

    /// The service `build` zoomed into, if it was found.
    pub fn focus(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// Best-connected, then largest, then by `(kind, name, node id)` — a total
    /// order, so ties never depend on insertion or hashing.
    fn rank_cmp(&self, a: u32, b: u32) -> std::cmp::Ordering {
        let (ga, gb) = (&self.groups[a as usize], &self.groups[b as usize]);
        self.degree[b as usize]
            .cmp(&self.degree[a as usize])
            .then_with(|| gb.contracts.cmp(&ga.contracts))
            .then_with(|| ga.kind.cmp(&gb.kind))
            .then_with(|| ga.name.cmp(&gb.name))
            .then_with(|| self.node_ids[a as usize].cmp(&self.node_ids[b as usize]))
    }

    /// Draws at most `max_groups` groups (in a zoom: `max_focus_contracts` of
    /// the service's contracts plus `max_groups` of the groups they link to)
    /// and folds the rest into one "other ..." node per kind.
    pub fn select(&self, opts: &TopologyOptions) -> Topology {
        const UNSEEN: u32 = u32::MAX;
        let max_groups = opts.max_groups.max(1);
        let shown: Vec<u32> = if self.focus.is_some() {
            let contracts = self
                .ranked
                .iter()
                .copied()
                .filter(|&g| self.groups[g as usize].kind == GroupKind::Contract)
                .take(opts.max_focus_contracts.max(1));
            // Neighbours ranked by how much they talk to *this* service, not by
            // their global degree (a hub linked once must not crowd out a
            // service linked fifty times).
            let mut neighbours: Vec<u32> = (0..self.groups.len() as u32)
                .filter(|&g| self.focus_weight[g as usize] > 0)
                .collect();
            neighbours.sort_unstable_by(|&a, &b| {
                self.focus_weight[b as usize]
                    .cmp(&self.focus_weight[a as usize])
                    .then_with(|| self.rank_cmp(a, b))
            });
            neighbours.truncate(max_groups);
            contracts.chain(neighbours).collect()
        } else {
            self.ranked.iter().copied().take(max_groups).collect()
        };

        let mut index = vec![UNSEEN; self.groups.len()];
        let mut out_groups: Vec<Group> = Vec::new();
        for g in shown {
            if index[g as usize] == UNSEEN {
                index[g as usize] = out_groups.len() as u32;
                out_groups.push(self.groups[g as usize].clone());
            }
        }

        // Fold everything else into one "other ..." node per kind.
        let mut hidden: Vec<(String, usize, GroupKind)> = Vec::new();
        let mut other_idx: BTreeMap<GroupKind, u32> = BTreeMap::new();
        for &g in &self.ranked {
            if index[g as usize] != UNSEEN {
                continue;
            }
            let group = &self.groups[g as usize];
            hidden.push((group.name.clone(), group.contracts, group.kind));
            let bucket = group.kind;
            let idx = *other_idx.entry(bucket).or_insert_with(|| {
                out_groups.push(Group {
                    name: match bucket {
                        GroupKind::Topic => "other topics".to_string(),
                        GroupKind::Contract => {
                            format!(
                                "other {} contracts",
                                self.focus.as_deref().unwrap_or_default()
                            )
                        }
                        _ => "other services".to_string(),
                    },
                    kind: GroupKind::Other,
                    contracts: 0,
                    by_kind: BTreeMap::new(),
                    internal_edges: 0,
                    folded: 0,
                });
                (out_groups.len() - 1) as u32
            });
            let other = &mut out_groups[idx as usize];
            other.contracts += group.contracts;
            other.folded += 1;
            index[g as usize] = idx;
        }
        hidden.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // Re-fold links onto drawn nodes; a link folded onto itself disappears.
        let mut folded: BTreeMap<(u32, u32, u8), (usize, u8, EdgeKind)> = BTreeMap::new();
        for l in &self.links {
            let (f, t) = (index[l.from as usize], index[l.to as usize]);
            if f == t {
                continue;
            }
            let entry = folded.entry((f, t, l.kind as u8)).or_insert((0, 0, l.kind));
            entry.0 += l.count;
            entry.1 = entry.1.max(l.weakest);
        }
        let links = folded
            .into_iter()
            .map(|((from, to, _), (count, conf, kind))| Link {
                from: from as usize,
                to: to as usize,
                kind,
                count,
                weakest: match conf {
                    0 => EdgeConfidence::Exact,
                    1 => EdgeConfidence::Heuristic,
                    _ => EdgeConfidence::Ambiguous,
                },
            })
            .collect();

        Topology {
            groups: out_groups,
            links,
            total_groups: self.groups.len(),
            total_contracts: self.total_contracts,
            total_edges: self.total_edges,
            hidden,
            grouping: if self.by_root { "root" } else { "package" },
            focus: self.focus.clone(),
        }
    }
}

/// Escapes text for a double-quoted Mermaid label with Mermaid entity codes:
/// a name from scanned source (topic literal, package, root) can neither end
/// the label (`"`), inject markup (`<`, `>`), forge an entity (`#`) nor break
/// the line-based syntax (newlines).
fn mermaid_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("#quot;"),
            '<' => out.push_str("#lt;"),
            '>' => out.push_str("#gt;"),
            '#' => out.push_str("#35;"),
            // A label opening with a backtick is a Mermaid markdown string.
            '`' => out.push_str("#96;"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Longest name drawn, in bytes. Topic names are string literals from scanned
/// source and package/root names are unbounded too: without a cap, one huge
/// name makes even the smallest view (`max_groups = 1`) exceed the 48 KB cap,
/// and the central truncation would then cut the JSON/HTML document in half.
pub const MAX_NAME_BYTES: usize = 120;

/// `s` shortened to [`MAX_NAME_BYTES`] (on a char boundary, marked with `…`).
pub fn display_name(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_NAME_BYTES {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut cut = MAX_NAME_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…", &s[..cut]))
}

/// A name inside Markdown inline code: no backtick can close it early and no
/// newline can start a new block.
pub fn inline_code_text(s: &str) -> String {
    display_name(s).replace('`', "'").replace(['\n', '\r'], " ")
}

impl Topology {
    pub fn build(graph: &ContractGraph, repo_names: &[String], opts: &TopologyOptions) -> Self {
        Aggregate::build(graph, repo_names, opts.focus.as_deref()).select(opts)
    }

    /// Node caption: `(title, detail)`, e.g. `("checkout", "30 interface · 503 class")`.
    fn caption(g: &Group) -> (String, Option<String>) {
        if g.kind == GroupKind::Other {
            return (
                format!(
                    "{} ({} groups, {} contracts)",
                    display_name(&g.name),
                    g.folded,
                    g.contracts
                ),
                None,
            );
        }
        let mut parts: Vec<String> = g
            .by_kind
            .iter()
            .rev()
            .map(|(k, n)| format!("{n} {k}"))
            .collect();
        parts.truncate(3);
        let detail = (!parts.is_empty()).then(|| parts.join(" · "));
        (display_name(&g.name).into_owned(), detail)
    }

    /// Mermaid flowchart of the aggregated topology.
    pub fn to_mermaid(&self, workspace_name: &str) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "%% CausalMesh service topology: {}\n",
            workspace_name.replace(['\n', '\r'], " ")
        ));
        out.push_str("graph LR\n");
        for (i, g) in self.groups.iter().enumerate() {
            let (title, detail) = Self::caption(g);
            let mut label = mermaid_text(&title);
            if let Some(d) = detail {
                label.push_str("<br/>");
                label.push_str(&mermaid_text(&d));
            }
            let (open, close) = match g.kind {
                GroupKind::Topic => ("([", "])"),
                GroupKind::Other => ("[/", "/]"),
                GroupKind::Contract => ("(", ")"),
                GroupKind::Service => ("[", "]"),
            };
            out.push_str(&format!("  g{i}{open}\"{label}\"{close}\n"));
        }
        for l in &self.links {
            let mut label = edge_label(l.kind).to_string();
            if l.count > 1 {
                label.push_str(&format!(" ×{}", l.count));
            }
            match l.weakest {
                EdgeConfidence::Heuristic => label.push_str(" (heuristic)"),
                EdgeConfidence::Ambiguous => label.push_str(" (ambiguous)"),
                EdgeConfidence::Exact => {}
            }
            let (arrow, tail) = match l.kind {
                EdgeKind::Produces | EdgeKind::DispatchesTo => ("==", "==>"),
                EdgeKind::Consumes => ("-.", ".->"),
                _ => ("--", "-->"),
            };
            out.push_str(&format!(
                "  g{} {arrow} \"{label}\" {tail} g{}\n",
                l.from, l.to
            ));
        }
        out
    }

    /// Web payload (for the JSON and HTML renderers) of the aggregated view.
    pub fn to_payload(&self, workspace_name: &str) -> WebGraphPayload {
        let nodes = self
            .groups
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let (title, detail) = Self::caption(g);
                let name = display_name(&g.name).into_owned();
                WebNode {
                    id: i as u32,
                    name: name.clone(),
                    kind: format!("{:?}", g.kind),
                    file_path: String::new(),
                    line_start: 0,
                    line_end: 0,
                    package: name.clone(),
                    repo: name,
                    signature: Some(match detail {
                        Some(d) => format!("{title} — {d}"),
                        None => title,
                    }),
                }
            })
            .collect::<Vec<_>>();
        let edges = self
            .links
            .iter()
            .map(|l| WebEdge {
                from: l.from as u32,
                to: l.to as u32,
                kind: format!("{:?}", l.kind),
                metadata: Some(format!("{} edge(s)", l.count)),
                confidence: l.weakest.label().to_string(),
            })
            .collect::<Vec<_>>();
        WebGraphPayload {
            workspace_name: workspace_name.to_string(),
            total_nodes: nodes.len(),
            total_edges: edges.len(),
            nodes,
            edges,
        }
    }

    /// Footer: what was folded and how to zoom, following the
    /// `smart_search` truncation affordance.
    pub fn footer(&self) -> String {
        let mut out = format!(
            "*{} contracts, {} edges folded into {} of {} groups (grouping: one service per {}).*\n",
            self.total_contracts,
            self.total_edges,
            self.groups.iter().filter(|g| g.kind != GroupKind::Other).count(),
            self.total_groups,
            self.grouping,
        );
        if !self.hidden.is_empty() {
            out.push_str(&format!(
                "\n{} groups are folded into \"other\". Largest hidden:\n",
                self.hidden.len()
            ));
            for (name, contracts, kind) in self.hidden.iter().take(5) {
                let what = match kind {
                    GroupKind::Topic => " — topic",
                    GroupKind::Contract => " — contract",
                    _ => "",
                };
                out.push_str(&format!(
                    "  - `{}` ({contracts} contracts{what})\n",
                    inline_code_text(name)
                ));
            }
        }
        if self.focus.is_none() {
            // Only a *service* can be zoomed into: never suggest a topic.
            let example = self
                .hidden
                .iter()
                .find(|(name, _, kind)| *kind == GroupKind::Service && name.len() <= MAX_NAME_BYTES)
                .map(|(name, _, _)| name.as_str())
                .or_else(|| {
                    self.groups
                        .iter()
                        .find(|g| g.kind == GroupKind::Service && g.name.len() <= MAX_NAME_BYTES)
                        .map(|g| g.name.as_str())
                });
            if let Some(name) = example {
                let quoted = serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""));
                out.push_str(&format!(
                    "\n👉 Zoom into one service: `visualize_mesh(service: {})`.\n",
                    quoted.replace('`', "'")
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{CompactStr, ContractEdge, FilePath};
    use std::path::Path;

    fn node(name: &str, pkg: &str, repo: RepoId, kind: NodeKind) -> ContractNode {
        ContractNode {
            id: 0,
            name: name.into(),
            kind,
            file_path: FilePath::from(Path::new(&format!("{pkg}/{name}.rs"))),
            line_start: 1,
            line_end: 1,
            package: CompactStr::new(pkg),
            repo_id: repo,
            signature: None,
            docstring: None,
        }
    }

    fn edge(from: NodeId, to: NodeId, kind: EdgeKind) -> ContractEdge {
        ContractEdge {
            from,
            to,
            kind,
            metadata: None,
            confidence: EdgeConfidence::Exact,
        }
    }

    fn two_root_graph() -> (ContractGraph, Vec<String>) {
        let mut g = ContractGraph::new();
        let a1 = g.add_node(node("OrderSvc", "orders", 0, NodeKind::GrpcService));
        let a2 = g.add_node(node("OrderRepo", "orders", 0, NodeKind::ServiceClass));
        let b1 = g.add_node(node("BillingClient", "billing", 1, NodeKind::ServiceClass));
        let b2 = g.add_node(node("Invoice", "billing", 1, NodeKind::ServiceClass));
        g.add_edge(edge(b1, a1, EdgeKind::CallsRpc));
        g.add_edge(edge(b2, a1, EdgeKind::CallsRpc));
        g.add_edge(edge(a2, a1, EdgeKind::Imports));
        (g, vec!["orders-svc".into(), "billing-svc".into()])
    }

    #[test]
    fn folds_contracts_per_root_and_weights_cross_service_links() {
        let (g, repos) = two_root_graph();
        let t = Topology::build(&g, &repos, &TopologyOptions::default());
        assert_eq!(t.grouping, "root");
        assert_eq!(t.groups.len(), 2);
        assert_eq!(t.links.len(), 1, "{:?}", t.links);
        assert_eq!(t.links[0].count, 2);
        let orders = t
            .groups
            .iter()
            .find(|g| g.name == "orders-svc")
            .expect("orders");
        assert_eq!(orders.internal_edges, 1);
        let m = t.to_mermaid("ws");
        assert!(m.contains("CallsRpc ×2"), "{m}");
    }

    #[test]
    fn caps_groups_and_folds_the_rest_into_other() {
        let mut g = ContractGraph::new();
        for i in 0..50 {
            g.add_node(node(
                &format!("C{i}"),
                &format!("pkg{i:02}"),
                0,
                NodeKind::ServiceClass,
            ));
        }
        let t = Topology::build(
            &g,
            &["only-root".into()],
            &TopologyOptions {
                max_groups: 10,
                ..Default::default()
            },
        );
        assert_eq!(t.grouping, "package");
        assert_eq!(t.groups.len(), 11, "10 shown + 1 other");
        let other = t.groups.last().expect("other");
        assert_eq!(other.kind, GroupKind::Other);
        assert_eq!(other.folded, 40);
        assert_eq!(t.hidden.len(), 40);
        assert!(t.footer().contains("visualize_mesh(service:"));
    }

    #[test]
    fn focus_keeps_the_service_and_its_neighbours_only() {
        let (mut g, mut repos) = two_root_graph();
        g.add_node(node("Lonely", "misc", 2, NodeKind::ServiceClass));
        repos.push("misc-svc".into());
        let t = Topology::build(
            &g,
            &repos,
            &TopologyOptions {
                focus: Some("BILLING-svc".into()),
                ..Default::default()
            },
        );
        assert_eq!(t.focus.as_deref(), Some("billing-svc"));
        let names: Vec<_> = t.groups.iter().map(|g| g.name.as_str()).collect();
        // The focused service is exploded into its own contracts...
        assert!(names.contains(&"BillingClient [class]"), "{names:?}");
        assert!(names.contains(&"Invoice [class]"), "{names:?}");
        // ...next to the services they talk to; unrelated ones are folded.
        assert!(names.contains(&"orders-svc"), "{names:?}");
        assert!(!names.contains(&"misc-svc"), "{names:?}");
        let m = t.to_mermaid("ws");
        assert!(m.contains("CallsRpc"), "{m}");
    }

    #[test]
    fn names_from_source_cannot_break_the_mermaid_label() {
        let mut g = ContractGraph::new();
        let a = g.add_node(node("A", "evil\"]\ng9[x", 0, NodeKind::ServiceClass));
        let b = g.add_node(node(
            "B",
            "<img src=x onerror=alert(1)>#quot;",
            0,
            NodeKind::ServiceClass,
        ));
        g.add_edge(edge(a, b, EdgeKind::CallsRpc));
        let t = Topology::build(&g, &["root".into()], &TopologyOptions::default());
        let m = t.to_mermaid("ws\ninjected");
        assert!(!m.contains("<img"), "{m}");
        assert!(m.contains("evil#quot;] g9[x"), "{m}");
        assert!(m.contains("#35;quot;"), "entity forged: {m}");
        // One comment line, one header, two nodes, one link — no injected line.
        assert_eq!(m.lines().count(), 5, "{m}");
    }

    #[test]
    fn focus_prefers_the_exact_case_match() {
        let mut g = ContractGraph::new();
        g.add_node(node("X", "Api", 0, NodeKind::ServiceClass));
        g.add_node(node("Y", "api", 0, NodeKind::ServiceClass));
        let zoom = |f: &str| {
            Topology::build(
                &g,
                &["root".into()],
                &TopologyOptions {
                    focus: Some(f.into()),
                    ..Default::default()
                },
            )
            .focus
        };
        assert_eq!(zoom("api").as_deref(), Some("api"));
        assert_eq!(zoom("Api").as_deref(), Some("Api"));
        assert_eq!(zoom("API").as_deref(), Some("Api"), "first in byte order");
    }

    #[test]
    fn footer_never_suggests_zooming_into_a_topic() {
        let mut g = ContractGraph::new();
        let s = g.add_node(node("Svc", "svc", 0, NodeKind::ServiceClass));
        for i in 0..5 {
            let mut t = node(&format!("topic.{i}"), "", RepoId::MAX, NodeKind::KafkaTopic);
            t.repo_id = RepoId::MAX;
            let t = g.add_node(t);
            g.add_edge(edge(s, t, EdgeKind::Produces));
        }
        let t = Topology::build(
            &g,
            &["root".into()],
            &TopologyOptions {
                max_groups: 1,
                ..Default::default()
            },
        );
        assert!(
            t.hidden.iter().all(|h| h.2 == GroupKind::Topic),
            "{:?}",
            t.hidden
        );
        let footer = t.footer();
        assert!(
            footer.contains("visualize_mesh(service: \"svc\")"),
            "{footer}"
        );
        assert!(footer.contains("— topic"), "{footer}");
    }

    #[test]
    fn zoom_ranks_neighbours_by_links_to_the_focused_service() {
        let mut g = ContractGraph::new();
        let f = g.add_node(node("F", "focus", 0, NodeKind::ServiceClass));
        // `hub` is linked once to the focus but to many others.
        let hub = g.add_node(node("H", "hub", 0, NodeKind::ServiceClass));
        g.add_edge(edge(f, hub, EdgeKind::CallsRpc));
        for i in 0..20 {
            let o = g.add_node(node(
                &format!("O{i}"),
                &format!("o{i:02}"),
                0,
                NodeKind::ServiceClass,
            ));
            g.add_edge(edge(o, hub, EdgeKind::CallsRpc));
        }
        // `peer` is linked to the focus three times.
        let peer = g.add_node(node("P", "peer", 0, NodeKind::ServiceClass));
        for _ in 0..3 {
            g.add_edge(edge(f, peer, EdgeKind::CallsRpc));
        }
        let t = Topology::build(
            &g,
            &["root".into()],
            &TopologyOptions {
                focus: Some("focus".into()),
                max_groups: 1,
                ..Default::default()
            },
        );
        let names: Vec<_> = t.groups.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"peer"), "{names:?}");
        assert!(!names.contains(&"hub"), "{names:?}");
    }

    /// The shrink loop builds the aggregate once and re-selects: that must be
    /// exactly what a from-scratch build gives.
    #[test]
    fn select_on_one_aggregate_matches_a_fresh_build() {
        let (g, repos) = two_root_graph();
        let agg = Aggregate::build(&g, &repos, None);
        for max_groups in [1, 2, 40] {
            let opts = TopologyOptions {
                max_groups,
                ..Default::default()
            };
            assert_eq!(
                agg.select(&opts).to_mermaid("ws"),
                Topology::build(&g, &repos, &opts).to_mermaid("ws")
            );
        }
    }

    /// A huge name (a topic literal, a package) is shortened everywhere it is
    /// drawn, so no single name can blow the 48 KB cap of the smallest view.
    #[test]
    fn huge_names_are_capped_in_every_rendering() {
        let huge = "x".repeat(100_000);
        let mut g = ContractGraph::new();
        let a = g.add_node(node("A", &huge, 0, NodeKind::ServiceClass));
        let b = g.add_node(node("B", "small", 0, NodeKind::ServiceClass));
        g.add_edge(edge(a, b, EdgeKind::CallsRpc));
        let t = Topology::build(&g, &["root".into()], &TopologyOptions::default());
        assert!(t.to_mermaid("ws").len() < 2048);
        assert!(t.footer().len() < 2048, "{}", t.footer());
        let json = serde_json::to_string(&t.to_payload("ws")).expect("json");
        assert!(json.len() < 4096, "{}", json.len());
        // A shortened name is never offered as a zoom target (it would not match).
        assert!(!t.footer().contains("xxx…\""), "{}", t.footer());
    }

    #[test]
    fn output_is_deterministic() {
        let (g, repos) = two_root_graph();
        let a = Topology::build(&g, &repos, &TopologyOptions::default()).to_mermaid("ws");
        let b = Topology::build(&g, &repos, &TopologyOptions::default()).to_mermaid("ws");
        assert_eq!(a, b);
    }
}
