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
    /// Largest hidden groups, `(name, contracts)`, biggest first.
    pub hidden: Vec<(String, usize)>,
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

/// Group key of one contract: `(name, kind)`.
fn group_of(node: &ContractNode, repo_names: &[String], by_root: bool) -> (String, GroupKind) {
    if is_topic_hub(node) {
        return (node.name.to_string(), GroupKind::Topic);
    }
    if by_root {
        if let Some(repo) = repo_names.get(node.repo_id as usize) {
            return (repo.clone(), GroupKind::Service);
        }
    }
    let pkg = if node.package.is_empty() {
        "shared".to_string()
    } else {
        node.package.to_string()
    };
    (pkg, GroupKind::Service)
}

impl Topology {
    pub fn build(graph: &ContractGraph, repo_names: &[String], opts: &TopologyOptions) -> Self {
        // One root → grouping by root would draw a single box; use packages.
        let mut roots_seen: Vec<RepoId> = graph
            .all_nodes()
            .filter(|n| !is_topic_hub(n) && (n.repo_id as usize) < repo_names.len())
            .map(|n| n.repo_id)
            .collect();
        roots_seen.sort_unstable();
        roots_seen.dedup();
        let by_root = roots_seen.len() > 1;

        // The zoomed service, matched case-insensitively against service names.
        let focus: Option<String> = opts.focus.as_deref().and_then(|f| {
            let mut names: Vec<String> = graph
                .all_nodes()
                .map(|n| group_of(n, repo_names, by_root))
                .filter(|(name, kind)| *kind == GroupKind::Service && name.eq_ignore_ascii_case(f))
                .map(|(name, _)| name)
                .collect();
            names.sort();
            names.into_iter().next()
        });

        // Unfolded groups keyed by (kind, key) for a deterministic order. In a
        // zoom, each contract of the focused service is its own group (key
        // made unique by node id, displayed by name).
        type Key = (GroupKind, String);
        let mut groups: BTreeMap<Key, Group> = BTreeMap::new();
        let mut node_group: HashMap<NodeId, Key> = HashMap::new();
        let mut total_contracts = 0usize;
        for node in graph.all_nodes() {
            let (name, kind) = group_of(node, repo_names, by_root);
            let (key, display, kind) =
                if kind == GroupKind::Service && focus.as_deref() == Some(name.as_str()) {
                    (
                        format!("{}\u{0}{}", node.name, node.id),
                        format!("{} [{}]", node.name, kind_label(node.kind)),
                        GroupKind::Contract,
                    )
                } else {
                    (name.clone(), name, kind)
                };
            let key = (kind, key);
            let g = groups.entry(key.clone()).or_insert_with(|| Group {
                name: display,
                kind,
                contracts: 0,
                by_kind: BTreeMap::new(),
                internal_edges: 0,
                folded: 0,
            });
            g.contracts += 1;
            if kind != GroupKind::Contract {
                *g.by_kind.entry(kind_label(node.kind)).or_default() += 1;
            }
            node_group.insert(node.id, key);
            total_contracts += 1;
        }

        // Cross-group edge weights: (from, to, kind) -> (count, weakest, kind).
        let mut weights: BTreeMap<(&Key, &Key, u8), (usize, u8, EdgeKind)> = BTreeMap::new();
        let mut degree: HashMap<&Key, usize> = HashMap::new();
        let edges = graph.all_edges();
        for e in edges {
            let (Some(from), Some(to)) = (node_group.get(&e.from), node_group.get(&e.to)) else {
                continue;
            };
            if from == to {
                if let Some(g) = groups.get_mut(from) {
                    g.internal_edges += 1;
                }
                continue;
            }
            let w = weights
                .entry((from, to, e.kind as u8))
                .or_insert((0, 0, e.kind));
            w.0 += 1;
            w.1 = w.1.max(confidence_rank(e.confidence));
            *degree.entry(from).or_default() += 1;
            *degree.entry(to).or_default() += 1;
        }

        // Ranking: best-connected, then largest, then by key.
        let mut ranked: Vec<&Key> = groups.keys().collect();
        ranked.sort_by(|a, b| {
            let da = degree.get(a).copied().unwrap_or(0);
            let db = degree.get(b).copied().unwrap_or(0);
            db.cmp(&da)
                .then_with(|| groups[*b].contracts.cmp(&groups[*a].contracts))
                .then_with(|| a.cmp(b))
        });

        // Drawn groups. Zoom: the focused service's best-connected contracts,
        // then the groups they link to. Otherwise: the top groups overall.
        let max_groups = opts.max_groups.max(1);
        let shown: Vec<&Key> = if focus.is_some() {
            let contracts: Vec<&Key> = ranked
                .iter()
                .copied()
                .filter(|k| k.0 == GroupKind::Contract)
                .take(opts.max_focus_contracts.max(1))
                .collect();
            let mut neighbours: Vec<&Key> = weights
                .keys()
                .filter_map(|(a, b, _)| match (a.0, b.0) {
                    (GroupKind::Contract, GroupKind::Contract) => None,
                    (GroupKind::Contract, _) => Some(*b),
                    (_, GroupKind::Contract) => Some(*a),
                    _ => None,
                })
                .collect();
            neighbours.sort_by(|a, b| {
                let da = degree.get(a).copied().unwrap_or(0);
                let db = degree.get(b).copied().unwrap_or(0);
                db.cmp(&da).then_with(|| a.cmp(b))
            });
            neighbours.dedup();
            neighbours.truncate(max_groups);
            contracts.into_iter().chain(neighbours).collect()
        } else {
            ranked.iter().copied().take(max_groups).collect()
        };

        let total_groups = groups.len();
        let mut out_groups: Vec<Group> = Vec::new();
        let mut index: HashMap<&Key, usize> = HashMap::new();
        for key in &shown {
            if index.contains_key(*key) {
                continue;
            }
            index.insert(*key, out_groups.len());
            out_groups.push(groups[*key].clone());
        }

        // Fold everything else into one "other ..." node per kind.
        let mut hidden: Vec<(String, usize)> = Vec::new();
        let mut other_idx: HashMap<GroupKind, usize> = HashMap::new();
        for key in &ranked {
            if index.contains_key(*key) {
                continue;
            }
            let g = &groups[*key];
            hidden.push((g.name.clone(), g.contracts));
            let bucket = g.kind;
            let idx = *other_idx.entry(bucket).or_insert_with(|| {
                out_groups.push(Group {
                    name: match bucket {
                        GroupKind::Topic => "other topics".to_string(),
                        GroupKind::Contract => {
                            format!("other {} contracts", focus.as_deref().unwrap_or_default())
                        }
                        _ => "other services".to_string(),
                    },
                    kind: GroupKind::Other,
                    contracts: 0,
                    by_kind: BTreeMap::new(),
                    internal_edges: 0,
                    folded: 0,
                });
                out_groups.len() - 1
            });
            let other = &mut out_groups[idx];
            other.contracts += g.contracts;
            other.folded += 1;
            index.insert(*key, idx);
        }
        hidden.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // Re-fold links onto drawn nodes; a link folded onto itself disappears.
        let mut folded: BTreeMap<(usize, usize, u8), (usize, u8, EdgeKind)> = BTreeMap::new();
        for ((from, to, kind_idx), (count, conf, kind)) in &weights {
            let (Some(&f), Some(&t)) = (index.get(*from), index.get(*to)) else {
                continue;
            };
            if f == t {
                continue;
            }
            let entry = folded.entry((f, t, *kind_idx)).or_insert((0, 0, *kind));
            entry.0 += count;
            entry.1 = entry.1.max(*conf);
        }
        let links = folded
            .into_iter()
            .map(|((from, to, _), (count, conf, kind))| Link {
                from,
                to,
                kind,
                count,
                weakest: match conf {
                    0 => EdgeConfidence::Exact,
                    1 => EdgeConfidence::Heuristic,
                    _ => EdgeConfidence::Ambiguous,
                },
            })
            .collect();

        Self {
            groups: out_groups,
            links,
            total_groups,
            total_contracts,
            total_edges: edges.len(),
            hidden,
            grouping: if by_root { "root" } else { "package" },
            focus,
        }
    }

    fn caption(g: &Group) -> String {
        let mut parts: Vec<String> = g
            .by_kind
            .iter()
            .rev()
            .map(|(k, n)| format!("{n} {k}"))
            .collect();
        parts.truncate(3);
        match g.kind {
            GroupKind::Other => format!(
                "{} ({} groups, {} contracts)",
                g.name, g.folded, g.contracts
            ),
            _ if parts.is_empty() => g.name.clone(),
            _ => format!("{}<br/>{}", g.name, parts.join(" · ")),
        }
    }

    fn sanitize(s: &str) -> String {
        s.replace('"', "'")
            .replace(['[', ']', '{', '}', '(', ')', '<', '>'], " ")
    }

    /// Mermaid flowchart of the aggregated topology.
    pub fn to_mermaid(&self, workspace_name: &str) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "%% CausalMesh service topology: {}\n",
            Self::sanitize(workspace_name)
        ));
        out.push_str("graph LR\n");
        for (i, g) in self.groups.iter().enumerate() {
            let label = Self::caption(g).replace('"', "'");
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
            let arrow = match l.kind {
                EdgeKind::Produces | EdgeKind::DispatchesTo => "==",
                EdgeKind::Consumes => "-.",
                _ => "--",
            };
            let tail = match l.kind {
                EdgeKind::Produces | EdgeKind::DispatchesTo => "==>",
                EdgeKind::Consumes => ".->",
                _ => "-->",
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
            .map(|(i, g)| WebNode {
                id: i as u32,
                name: g.name.clone(),
                kind: format!("{:?}", g.kind),
                file_path: String::new(),
                line_start: 0,
                line_end: 0,
                package: g.name.clone(),
                repo: g.name.clone(),
                signature: Some(Self::caption(g).replace("<br/>", " — ")),
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
            for (name, contracts) in self.hidden.iter().take(5) {
                out.push_str(&format!("  - `{name}` ({contracts} contracts)\n"));
            }
        }
        let example = self.hidden.first().map(|(n, _)| n.clone()).or_else(|| {
            self.groups
                .iter()
                .find(|g| g.kind == GroupKind::Service)
                .map(|g| g.name.clone())
        });
        if self.focus.is_none() {
            if let Some(name) = example {
                out.push_str(&format!(
                    "\n👉 Zoom into one service: `visualize_mesh(service: \"{name}\")`.\n"
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
    fn output_is_deterministic() {
        let (g, repos) = two_root_graph();
        let a = Topology::build(&g, &repos, &TopologyOptions::default()).to_mermaid("ws");
        let b = Topology::build(&g, &repos, &TopologyOptions::default()).to_mermaid("ws");
        assert_eq!(a, b);
    }
}
