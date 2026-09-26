use crate::types::CompactStr;
use crate::types::{
    ContractEdge, ContractNode, EdgeConfidence, EdgeKind, FilePath, NodeId, NodeKind, RepoId,
};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Query results borrow from the graph: the snapshot guard held by the caller
/// keeps them alive, and cloning every `ContractNode` (with its `PathBuf`) per
/// request is what the formatter never needed.
#[derive(Debug, Clone, Serialize)]
pub struct GrpcTrace<'g> {
    pub target: CompactStr,
    pub proto_definition: Option<&'g ContractNode>,
    /// Each stub paired with the confidence of the match that linked it to
    /// the proto definition (bare-name scan vs. an exact `CallsRpc` edge).
    pub client_stubs: Vec<(&'g ContractNode, EdgeConfidence)>,
    /// Each handler paired with the confidence of the match that linked it
    /// to the proto definition (bare-name scan vs. an exact `Implements` edge).
    pub server_handlers: Vec<(&'g ContractNode, EdgeConfidence)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactFlow<'g> {
    pub target: CompactStr,
    pub upstream_producers: Vec<&'g ContractNode>,
    pub topics: Vec<&'g ContractNode>,
    pub downstream_consumers: Vec<&'g ContractNode>,
    pub related_sagas: Vec<&'g ContractNode>,
}

/// In-memory graph of polyglot architecture contracts and dependencies.
#[derive(Debug, Clone, Default)]
pub struct ContractGraph {
    // `BTreeMap`, not `HashMap`: std's `HashMap` uses a per-process-random `SipHash`
    // seed (`RandomState`), so iterating `.values()` — done below to resolve an
    // ambiguous target (multiple proto methods sharing a bare name, multiple RPC
    // call candidates, `analyze_grpc`'s anchor scan) — visits nodes in a different
    // order on every process run even for byte-identical input. `BTreeMap` iterates
    // in sorted `NodeId` order, which is itself deterministic (ids are assigned
    // sequentially while folding files in the indexer's now-sorted crawl order), so
    // "first candidate encountered" becomes reproducible instead of a coin flip
    // seeded at process start (idempotence invariant I1).
    nodes: BTreeMap<NodeId, ContractNode>,
    edges: Vec<ContractEdge>,

    // O(1) in-memory indices
    name_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    package_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    file_to_nodes: HashMap<FilePath, Vec<NodeId>>,
    fqcn_to_node: HashMap<CompactStr, Vec<NodeId>>,
    reverse_deps: HashMap<CompactStr, Vec<NodeId>>,
    topic_producers: HashMap<CompactStr, Vec<NodeId>>,
    topic_consumers: HashMap<CompactStr, Vec<NodeId>>,
    rpc_calls: Vec<(NodeId, CompactStr)>,

    next_node_id: u32,
}

impl ContractGraph {
    /// Hard ceiling on `analyze_impact_with_depth`'s hop count. Bounds the BFS
    /// walk independent of cycle detection — a caller passing an inflated depth
    /// on a very connected mesh should not turn "impact analysis" into "flood
    /// the payload budget", mirroring the other bounded-traversal caps in this
    /// crate (AST nesting depth, query cursor step limit).
    const MAX_IMPACT_DEPTH: usize = 5;

    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn get_node(&self, id: NodeId) -> Option<&ContractNode> {
        self.nodes.get(&id)
    }

    #[inline]
    pub fn all_nodes(&self) -> impl Iterator<Item = &ContractNode> {
        self.nodes.values()
    }

    #[inline]
    pub fn all_edges(&self) -> &[ContractEdge] {
        &self.edges
    }

    pub fn get_nodes_for_file(&self, path: &Path) -> &[NodeId] {
        self.file_to_nodes
            .get(path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn add_node(&mut self, mut node: ContractNode) -> NodeId {
        let id = self.next_node_id;
        self.next_node_id += 1;
        node.id = id;

        self.name_to_nodes
            .entry(node.name.clone())
            .or_default()
            .push(id);

        if !node.package.is_empty() {
            self.package_to_nodes
                .entry(node.package.clone())
                .or_default()
                .push(id);
        }

        self.file_to_nodes
            .entry(node.file_path.clone())
            .or_default()
            .push(id);

        if node.kind == NodeKind::GrpcMethod
            || node.kind == NodeKind::GrpcService
            || !node.package.is_empty()
        {
            let fqcn = CompactStr::new(format!("{}/{}", node.package, node.name));
            self.fqcn_to_node.entry(fqcn).or_default().push(id);
        }

        if node.kind == NodeKind::KafkaTopic
            || node.kind == NodeKind::EventStream
            || node.kind == NodeKind::Queue
        {
            let topic_key = node.name.to_lowercase();
            self.name_to_nodes
                .entry(CompactStr::new(&topic_key))
                .or_default()
                .push(id);
        }

        self.nodes.insert(id, node);
        id
    }

    pub fn add_edge(&mut self, edge: ContractEdge) {
        match edge.kind {
            EdgeKind::Produces => {
                if let Some(to_node) = self.nodes.get(&edge.to) {
                    let key = CompactStr::new(to_node.name.to_lowercase());
                    self.topic_producers.entry(key).or_default().push(edge.from);
                }
            }
            EdgeKind::Consumes => {
                if let Some(to_node) = self.nodes.get(&edge.to) {
                    let key = CompactStr::new(to_node.name.to_lowercase());
                    self.topic_consumers.entry(key).or_default().push(edge.from);
                }
            }
            _ => {}
        }
        self.edges.push(edge);
    }

    /// Records the raw fact "`consumer_node_id` imports `imported_target`". This is
    /// the *only* place that fact is stored — no placeholder edge is created here.
    /// `reconcile_edges` derives every `Imports` edge fresh, every time it runs, by
    /// reading `reverse_deps` directly; that is what lets an incremental reload of
    /// some *other* file still produce the correct edge for this (unchanged)
    /// consumer without `consumer_node_id` itself being re-folded (idempotence
    /// invariants I2/I3 — see the note on `reconcile_edges`).
    pub fn add_dependency(&mut self, consumer_node_id: NodeId, imported_target: &str) {
        let key = CompactStr::new(imported_target);
        self.reverse_deps
            .entry(key)
            .or_default()
            .push(consumer_node_id);
    }

    pub fn add_producer(&mut self, producer_node_id: NodeId, topic: &str) {
        let key = CompactStr::new(topic.to_lowercase());
        self.topic_producers
            .entry(key)
            .or_default()
            .push(producer_node_id);
    }

    pub fn add_consumer(&mut self, consumer_node_id: NodeId, topic: &str) {
        let key = CompactStr::new(topic.to_lowercase());
        self.topic_consumers
            .entry(key)
            .or_default()
            .push(consumer_node_id);
    }

    pub fn add_rpc_call(&mut self, caller_node_id: NodeId, target_rpc: &str) {
        self.rpc_calls
            .push((caller_node_id, CompactStr::new(target_rpc)));
    }

    pub fn patch_file(&mut self, file_path: &Path) {
        self.patch_files(std::iter::once(file_path));
    }

    /// Purges every node declared in any of `file_paths` in a single pass over the
    /// secondary indices, instead of one full `retain` sweep per file.
    pub fn patch_files<'a>(&mut self, file_paths: impl IntoIterator<Item = &'a Path>) {
        let mut stale_set: HashSet<NodeId> = HashSet::new();
        for path in file_paths {
            if let Some(ids) = self.file_to_nodes.remove(path) {
                stale_set.extend(ids);
            }
        }
        if stale_set.is_empty() {
            return;
        }

        // Targeted removal from the indices keyed by a node attribute we know.
        for id in &stale_set {
            let Some(node) = self.nodes.remove(id) else {
                continue;
            };
            Self::remove_from_index(&mut self.name_to_nodes, &node.name, *id);
            if node.kind == NodeKind::KafkaTopic
                || node.kind == NodeKind::EventStream
                || node.kind == NodeKind::Queue
            {
                let key = CompactStr::new(node.name.to_lowercase());
                Self::remove_from_index(&mut self.name_to_nodes, &key, *id);
            }
            if !node.package.is_empty() {
                Self::remove_from_index(&mut self.package_to_nodes, &node.package, *id);
            }
            if node.kind == NodeKind::GrpcMethod
                || node.kind == NodeKind::GrpcService
                || !node.package.is_empty()
            {
                let fqcn = CompactStr::new(format!("{}/{}", node.package, node.name));
                Self::remove_from_index(&mut self.fqcn_to_node, &fqcn, *id);
            }
        }

        // Indices keyed by *target* (not node) need one sweep — done once for the whole batch.
        for map in [
            &mut self.reverse_deps,
            &mut self.topic_producers,
            &mut self.topic_consumers,
        ] {
            map.retain(|_, ids| {
                ids.retain(|id| !stale_set.contains(id));
                !ids.is_empty()
            });
        }
        self.rpc_calls.retain(|(id, _)| !stale_set.contains(id));
        self.edges
            .retain(|e| !stale_set.contains(&e.from) && !stale_set.contains(&e.to));
    }

    fn remove_from_index(
        index: &mut HashMap<CompactStr, Vec<NodeId>>,
        key: &CompactStr,
        id: NodeId,
    ) {
        if let Some(ids) = index.get_mut(key) {
            ids.retain(|x| *x != id);
            if ids.is_empty() {
                index.remove(key);
            }
        }
    }

    /// Fully removes one node (by id) from `self.nodes` and every secondary index
    /// that references it — the same per-node cleanup `patch_files` does for a
    /// whole file's worth of stale nodes, available standalone for garbage
    /// collecting a single synthetic node (a `reconcile_edges`-created topic hub
    /// that no longer has any producer or consumer backing it; see the note there).
    fn remove_node(&mut self, id: NodeId) {
        let Some(node) = self.nodes.remove(&id) else {
            return;
        };
        Self::remove_from_index(&mut self.name_to_nodes, &node.name, id);
        if node.kind == NodeKind::KafkaTopic
            || node.kind == NodeKind::EventStream
            || node.kind == NodeKind::Queue
        {
            let key = CompactStr::new(node.name.to_lowercase());
            Self::remove_from_index(&mut self.name_to_nodes, &key, id);
        }
        if !node.package.is_empty() {
            Self::remove_from_index(&mut self.package_to_nodes, &node.package, id);
        }
        if node.kind == NodeKind::GrpcMethod
            || node.kind == NodeKind::GrpcService
            || !node.package.is_empty()
        {
            let fqcn = CompactStr::new(format!("{}/{}", node.package, node.name));
            Self::remove_from_index(&mut self.fqcn_to_node, &fqcn, id);
        }
        if let Some(ids) = self.file_to_nodes.get_mut(&node.file_path) {
            ids.retain(|x| *x != id);
            if ids.is_empty() {
                self.file_to_nodes.remove(&node.file_path);
            }
        }
    }

    /// Splits a fully-qualified identifier into `(package, bare_name)`.
    ///
    /// Covers Java-style dotted FQCNs (`com.acme.billing.Invoice`) and
    /// Rust-style `::`-separated paths (`crate::billing::Invoice`). `::` is
    /// checked first since it never appears in a dotted path and a Rust path
    /// segment can itself legitimately contain a `.` (rare, but this keeps
    /// the two syntaxes from being conflated).
    ///
    /// The Rust-style package is normalized from `::` to `.` before being
    /// returned, since `ContractNode::package` is always stored dotted
    /// (`detect_service_package`, proto `package` statements, Java FQCNs).
    /// Without this normalization a `crate::billing::Invoice` import could
    /// never match a node whose package is recorded as `crate.billing`.
    fn split_fully_qualified(target_str: &str) -> Option<(Cow<'_, str>, &str)> {
        if let Some(idx) = target_str.rfind("::") {
            let (pkg, name) = (&target_str[..idx], &target_str[idx + 2..]);
            if !pkg.is_empty() && !name.is_empty() {
                return Some((Cow::Owned(pkg.replace("::", ".")), name));
            }
        }
        if let Some((pkg, name)) = target_str.rsplit_once('.') {
            if !pkg.is_empty() && !name.is_empty() {
                return Some((Cow::Borrowed(pkg), name));
            }
        }
        None
    }

    /// `ids.len() == 1`, or exactly one candidate whose `repo_id` matches
    /// `importer_repo` — a legitimate, non-arbitrary tiebreak — returns that one
    /// candidate at `confidence_if_unique`. Otherwise this is a genuine tie the
    /// graph cannot resolve on its own: every candidate (or every same-repo
    /// candidate, when more than one shares the importer's repo) is returned
    /// tagged `Ambiguous`, rather than arbitrarily picking whichever one the
    /// caller's index happened to list first. That pick used to depend on
    /// `HashMap` iteration order and file processing order — a hidden source of
    /// nondeterminism (idempotence invariant I1) as well as a silently wrong
    /// answer for a real homonym (e.g. two proto packages each declaring their
    /// own `AdminService`).
    fn pick_or_ambiguous(
        &self,
        ids: &[NodeId],
        importer_repo: Option<RepoId>,
        confidence_if_unique: EdgeConfidence,
    ) -> Vec<(NodeId, EdgeConfidence)> {
        if let [only] = ids {
            return vec![(*only, confidence_if_unique)];
        }
        let same_repo: Vec<NodeId> = ids
            .iter()
            .copied()
            .filter(|id| self.nodes.get(id).map(|n| n.repo_id) == importer_repo)
            .collect();
        match same_repo.as_slice() {
            [only] => vec![(*only, confidence_if_unique)],
            [] => ids
                .iter()
                .map(|&id| (id, EdgeConfidence::Ambiguous))
                .collect(),
            _ => same_repo
                .into_iter()
                .map(|id| (id, EdgeConfidence::Ambiguous))
                .collect(),
        }
    }

    /// Same shape as [`Self::pick_or_ambiguous`], grouping by declared `package`
    /// instead of `repo_id` — used to disambiguate an RPC call's bare-method-name
    /// candidates by the caller's own package.
    fn pick_or_ambiguous_by_package(
        &self,
        ids: &[NodeId],
        caller_package: &CompactStr,
        confidence_if_unique: EdgeConfidence,
    ) -> Vec<(NodeId, EdgeConfidence)> {
        if let [only] = ids {
            return vec![(*only, confidence_if_unique)];
        }
        let same_package: Vec<NodeId> = ids
            .iter()
            .copied()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|n| n.package == *caller_package)
            })
            .collect();
        match same_package.as_slice() {
            [only] => vec![(*only, confidence_if_unique)],
            [] => ids
                .iter()
                .map(|&id| (id, EdgeConfidence::Ambiguous))
                .collect(),
            _ => same_package
                .into_iter()
                .map(|id| (id, EdgeConfidence::Ambiguous))
                .collect(),
        }
    }

    /// Resolves an `Imports` fact (`importer` imports `target_str`) using the O(1)
    /// indices. Tries each strategy in priority order and returns as soon as one
    /// produces any candidate(s) — one unambiguous winner, or every tied candidate
    /// tagged `Ambiguous` (see [`Self::pick_or_ambiguous`]). An empty result means
    /// no strategy matched at all (e.g. an external package like `@nestjs/common`).
    fn resolve_import_targets(
        &self,
        importer: NodeId,
        target_str: &str,
    ) -> Vec<(NodeId, EdgeConfidence)> {
        let is_relative_or_absolute_path =
            target_str.starts_with('.') || target_str.starts_with('/');
        let importer_repo = self.nodes.get(&importer).map(|n| n.repo_id);

        // 1. Fully-qualified name (Java `a.b.C`, Rust `a::b::C`, gRPC
        // `package/Name`): an exact package+name pair is unambiguous, so
        // this is tried first and is the only strategy tagged `Exact`.
        if !is_relative_or_absolute_path {
            if let Some((pkg, name)) = Self::split_fully_qualified(target_str) {
                if let Some(ids) = self.name_to_nodes.get(name) {
                    if let Some(&id) = ids.iter().find(|id| {
                        self.nodes
                            .get(id)
                            .is_some_and(|n| n.package.as_str() == pkg.as_ref())
                    }) {
                        return vec![(id, EdgeConfidence::Exact)];
                    }
                }
                let fqcn_key = format!("{pkg}/{name}");
                if let Some(ids) = self.fqcn_to_node.get(fqcn_key.as_str()) {
                    return self.pick_or_ambiguous(ids, importer_repo, EdgeConfidence::Exact);
                }
            }
        }

        // 2. Exact symbol name anywhere in the mesh — a bare name, so two
        // unrelated types sharing it would collide; heuristic.
        // Prioritize a symbol in the same repository as the importer to avoid cross-repo false links.
        if let Some(ids) = self.name_to_nodes.get(target_str) {
            return self.pick_or_ambiguous(ids, importer_repo, EdgeConfidence::Heuristic);
        }

        let is_qualified = target_str.contains('/') || target_str.contains('.');

        // 3. Relative import: match on file stem, same repo as the importer only
        // (no cross-repo fallback — a relative import can never legitimately
        // resolve outside the importer's own repository).
        if is_relative_or_absolute_path {
            let target_stem = Path::new(target_str)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(target_str);
            if let Some(ids) = self.name_to_nodes.get(target_stem) {
                let same_repo: Vec<NodeId> = ids
                    .iter()
                    .copied()
                    .filter(|id| self.nodes.get(id).map(|n| n.repo_id) == importer_repo)
                    .collect();
                if !same_repo.is_empty() {
                    return self.pick_or_ambiguous(
                        &same_repo,
                        importer_repo,
                        EdgeConfidence::Heuristic,
                    );
                }
            }
        }

        // 4. Qualified package identifier (e.g. '@scope/pkg', 'com.acme.billing')
        // matched against a whole package, not a specific symbol; heuristic.
        // Prioritize package nodes in the importer's repository if available.
        if is_qualified && !is_relative_or_absolute_path {
            if let Some(ids) = self.package_to_nodes.get(target_str) {
                return self.pick_or_ambiguous(ids, importer_repo, EdgeConfidence::Heuristic);
            }
        }

        Vec::new()
    }

    /// Reconciles causal edges across microservices:
    /// 1. Derives `Imports` edges fresh from the raw `reverse_deps` facts.
    /// 2. Links topic producers & consumers to canonical Kafka/Event topic nodes.
    /// 3. Creates direct causal dispatch edges from producers to downstream consumers.
    /// 4. Links service handlers to protobuf RPC declarations (EdgeKind::Implements).
    /// 5. Links RPC client calls to proto methods (EdgeKind::CallsRpc).
    ///
    /// Every lookup goes through the O(1) indices and edge de-duplication uses a
    /// `HashSet`, so the whole pass is linear in nodes + edges (was O(E²)).
    ///
    /// The whole edge set is **cleared and rebuilt from scratch** on every call,
    /// rather than mutating whatever `self.edges` already held: an incremental
    /// reload only re-folds the *changed* files' facts (`WorkspaceIndexer::reload`),
    /// so an unchanged file's edge to a just-reindexed target would otherwise stay
    /// stale (pointing at a node `patch_files` already removed) or vanish entirely
    /// — silently, since that file was never touched this cycle to notice. Rebuilding
    /// from the raw fact indices (`reverse_deps`, `topic_producers`, `topic_consumers`,
    /// `rpc_calls`, and the node-kind scans below), which `patch_files` keeps current
    /// for every *surviving* node regardless of what changed, makes one full rebuild
    /// and any incremental reload converge to the same graph (idempotence invariants
    /// I2 and I3: `derive(derive(g)) == derive(g)`).
    pub fn reconcile_edges(&mut self) {
        self.edges.clear();
        let mut edge_set: HashSet<(NodeId, NodeId, EdgeKind)> = HashSet::new();

        // 1. Imports: resolve every (target, importer) raw fact fresh. Sorted so the
        // edge push order — irrelevant to `canonical_lines()`, which sorts its own
        // output, but a stale node id surviving in `reverse_deps` past its own
        // removal (shouldn't happen; `patch_files` prunes it) can't panic either way
        // since resolution only ever looks up ids still present in `self.nodes`.
        let mut import_facts: Vec<(NodeId, &CompactStr)> = self
            .reverse_deps
            .iter()
            .flat_map(|(target, ids)| ids.iter().map(move |&id| (id, target)))
            .collect();
        import_facts.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
        let mut import_edges = Vec::new();
        for (importer_id, target) in import_facts {
            if !self.nodes.contains_key(&importer_id) {
                continue;
            }
            for (to_id, confidence) in self.resolve_import_targets(importer_id, target.as_str()) {
                if edge_set.insert((importer_id, to_id, EdgeKind::Imports)) {
                    import_edges.push(ContractEdge {
                        from: importer_id,
                        to: to_id,
                        kind: EdgeKind::Imports,
                        metadata: Some(target.clone()),
                        confidence,
                    });
                }
            }
        }
        self.edges.extend(import_edges);

        // 2 & 3. Topic hubs and causal dispatch.
        let mut all_topics: Vec<CompactStr> = self
            .topic_producers
            .keys()
            .chain(self.topic_consumers.keys())
            .cloned()
            .collect();
        all_topics.sort_unstable();
        all_topics.dedup();

        // A synthetic hub node from a *previous* reconcile call whose topic no
        // longer has any producer or consumer fact (the file that used to publish
        // it was edited to use a different topic, or deleted) must not linger
        // forever: a full rebuild from the current facts alone would never create
        // it, so leaving it in place would make an incremental reload permanently
        // diverge from a full rebuild (idempotence invariant I2).
        let live_topics: HashSet<&CompactStr> = all_topics.iter().collect();
        let stale_hubs: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| {
                n.package == "event-bus"
                    && matches!(
                        n.kind,
                        NodeKind::EventStream | NodeKind::KafkaTopic | NodeKind::Queue
                    )
                    && !live_topics.contains(&n.name)
            })
            .map(|n| n.id)
            .collect();
        for id in stale_hubs {
            self.remove_node(id);
        }

        for topic_key in all_topics {
            // `add_node` indexes stream-like nodes under their lowercase name, and
            // `topic_key` is already lowercase.
            let existing_topic_id = self.name_to_nodes.get(&topic_key).and_then(|ids| {
                ids.iter().copied().find(|id| {
                    self.nodes.get(id).is_some_and(|n| {
                        n.package == "event-bus" && n.kind == NodeKind::EventStream
                    })
                })
            });

            let topic_id = match existing_topic_id {
                Some(id) => id,
                None => {
                    // Declarative `[[engines.contracts.patterns]]` hits are transport-agnostic
                    // (Kafka, Redis Streams, BullMQ, SQS, ...) — label them EventStream, not
                    // KafkaTopic. Real Kafka usage is still tagged NodeKind::KafkaTopic by the
                    // language-specific extractors that actually detect it (e.g. Spring @KafkaListener).
                    let node = ContractNode {
                        id: 0,
                        name: topic_key.clone(),
                        kind: NodeKind::EventStream,
                        file_path: FilePath::from(Path::new("event-bus")),
                        line_start: 1,
                        line_end: 1,
                        package: CompactStr::new("event-bus"),
                        // This node is a synthetic cross-repo hub, not something
                        // scanned from any configured root — `repo_id: 0` would
                        // silently alias it to whichever repo happens to be
                        // first in `[workspace] roots`, misattributing every
                        // event/topic in the mesh to that one repo. `RepoId::MAX`
                        // is a sentinel no real root index can ever reach.
                        repo_id: RepoId::MAX,
                        signature: Some(CompactStr::new(format!("topic://{topic_key}"))),
                        docstring: None,
                    };
                    self.add_node(node)
                }
            };

            let producers = self
                .topic_producers
                .get(&topic_key)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let consumers = self
                .topic_consumers
                .get(&topic_key)
                .map(Vec::as_slice)
                .unwrap_or(&[]);

            for &prod_id in producers {
                if edge_set.insert((prod_id, topic_id, EdgeKind::Produces)) {
                    self.edges.push(ContractEdge {
                        from: prod_id,
                        to: topic_id,
                        kind: EdgeKind::Produces,
                        metadata: Some(topic_key.clone()),
                        // Structural: derived directly from producer registration,
                        // not a name-matching heuristic.
                        confidence: EdgeConfidence::Exact,
                    });
                }
            }

            for &cons_id in consumers {
                if edge_set.insert((topic_id, cons_id, EdgeKind::Consumes)) {
                    self.edges.push(ContractEdge {
                        from: topic_id,
                        to: cons_id,
                        kind: EdgeKind::Consumes,
                        metadata: Some(topic_key.clone()),
                        // Structural: derived directly from consumer registration.
                        confidence: EdgeConfidence::Exact,
                    });
                }
            }

            // Direct `Produces` and `Consumes` edges carry topic routing via a
            // two-hop producer -> topic -> consumer walk with linear O(producers + consumers)
            // edge complexity, rather than quadratic O(producers * consumers) edge growth.
        }

        // 4. Reconcile gRPC Proto Definitions <-> Service Handlers (Implements)
        let proto_methods: Vec<(NodeId, CompactStr)> = self
            .nodes
            .values()
            .filter(|n| n.kind == NodeKind::GrpcMethod && Self::is_proto_file(&n.file_path))
            .map(|n| (n.id, n.name.clone()))
            .collect();

        // Only nodes the language extractors actually tagged as a gRPC handler
        // (e.g. TS `@GrpcMethod`/`@GrpcService`) may implement an RPC. Without
        // this, any same-named REST handler, factory method, or unrelated
        // function matches by bare name alone. Collected once, not per proto method.
        struct Handler<'a> {
            id: NodeId,
            name: &'a str,
            pascal_name: String,
            signature: Option<&'a str>,
        }
        let handlers: Vec<Handler<'_>> = self
            .nodes
            .values()
            .filter(|n| {
                (n.kind == NodeKind::GrpcMethod || n.kind == NodeKind::GrpcService)
                    && !Self::is_proto_file(&n.file_path)
                    && !n.name.starts_with("rpc:")
                    && !n.name.starts_with("produce:")
                    && !n.name.starts_with("consume:")
            })
            .map(|n| Handler {
                id: n.id,
                name: n.name.as_str(),
                pascal_name: crate::types::to_pascal_case(n.name.as_str()),
                signature: n.signature.as_deref(),
            })
            .collect();

        let mut new_edges = Vec::new();
        for (proto_id, method_fqcn) in &proto_methods {
            let bare = method_fqcn
                .split('.')
                .next_back()
                .unwrap_or(method_fqcn.as_str());

            for h in &handlers {
                // Only an exact match against the full FQCN is trustworthy;
                // case-folding, PascalCase normalization, and substring
                // signature scraping are all bare-name heuristics that can
                // collide across unrelated same-named symbols.
                let confidence = if h.name == method_fqcn.as_str() {
                    Some(EdgeConfidence::Exact)
                } else if h.name.eq_ignore_ascii_case(bare)
                    || h.pascal_name == bare
                    || Self::bare_names_match(h.name, bare)
                    || h.signature
                        .is_some_and(|s| Self::signature_contains_bare(s, bare))
                {
                    Some(EdgeConfidence::Heuristic)
                } else {
                    None
                };

                let Some(confidence) = confidence else {
                    continue;
                };
                if edge_set.insert((h.id, *proto_id, EdgeKind::Implements)) {
                    new_edges.push(ContractEdge {
                        from: h.id,
                        to: *proto_id,
                        kind: EdgeKind::Implements,
                        metadata: Some(method_fqcn.clone()),
                        confidence,
                    });
                }
            }
        }
        self.edges.extend(new_edges);

        // 5. Reconcile RPC Client Calls (CallsRpc) — proto methods indexed by
        // lowercase full and bare name; first declaration wins, as before.
        //
        // Also indexed here: every declared `GrpcService` node, proto-anchored
        // or not. A client-side call site (e.g. Go's `pb.NewFooServiceClient(conn)`
        // construction) records the *service* it talks to, not a specific proto
        // method FQCN — there is no proto method to name at that call site, only
        // the service. Matching directly against the service's own name (its
        // .proto declaration when indexed, or a language extractor's own
        // service-implementation node — e.g. Go's `RegisterFooServiceServer`-
        // detected node — when it isn't) is what lets `find_dependents`/
        // `analyze_grpc` resolve a real cross-service caller even when the
        // repo's `.proto` sources aren't in `roots` at all.
        // Both indexed by every matching candidate (not just the first found) —
        // a bare service name like "AdminService" is exactly the shape two
        // unrelated proto packages can legitimately both declare, and picking
        // only the first one `self.nodes.values()` (or file processing order)
        // happened to insert used to be a silent, order-dependent wrong answer
        // (idempotence invariant I1). Real ties are surfaced via
        // `pick_or_ambiguous_by_package` below instead.
        let mut proto_by_fqcn: HashMap<String, Vec<NodeId>> =
            HashMap::with_capacity(proto_methods.len() * 2);
        let mut proto_by_bare: HashMap<String, Vec<NodeId>> =
            HashMap::with_capacity(proto_methods.len());

        for (id, name) in &proto_methods {
            proto_by_fqcn
                .entry(name.as_str().to_lowercase())
                .or_default()
                .push(*id);
            if let Some(bare) = name.split('.').next_back() {
                proto_by_bare
                    .entry(bare.to_lowercase())
                    .or_default()
                    .push(*id);
            }
        }
        for node in self.nodes.values() {
            if node.kind == NodeKind::GrpcService {
                proto_by_fqcn
                    .entry(node.name.as_str().to_lowercase())
                    .or_default()
                    .push(node.id);
            }
        }

        let mut rpc_edges = Vec::new();
        for (caller_id, target_rpc) in &self.rpc_calls {
            let target_str = target_rpc.as_str();
            let target_lower = target_str.to_lowercase();
            let target_bare = target_str.split('.').next_back().unwrap_or(target_str);
            let target_bare_lower = target_bare.to_lowercase();

            // 1. Exact FQCN match has top priority and highest confidence.
            // 2. Bare-name fallback: lower confidence.
            // At either tier, disambiguate multiple candidates by caller package;
            // a genuine tie (no candidate shares the caller's package, or more than
            // one does) fans out to every tied candidate as `Ambiguous` rather than
            // picking whichever one the index happened to list first.
            let caller_package = self.nodes.get(caller_id).map(|n| n.package.clone());
            let matches: Vec<(NodeId, EdgeConfidence)> = if let Some(candidates) =
                proto_by_fqcn.get(&target_lower)
            {
                match &caller_package {
                    Some(pkg) => {
                        self.pick_or_ambiguous_by_package(candidates, pkg, EdgeConfidence::Exact)
                    }
                    None => candidates
                        .iter()
                        .map(|&id| (id, EdgeConfidence::Exact))
                        .collect(),
                }
            } else if let Some(candidates) = proto_by_bare.get(&target_bare_lower) {
                match &caller_package {
                    Some(pkg) => self.pick_or_ambiguous_by_package(
                        candidates,
                        pkg,
                        EdgeConfidence::Heuristic,
                    ),
                    None => candidates
                        .iter()
                        .map(|&id| (id, EdgeConfidence::Ambiguous))
                        .collect(),
                }
            } else {
                Vec::new()
            };

            for (target_id, confidence) in matches {
                if edge_set.insert((*caller_id, target_id, EdgeKind::CallsRpc)) {
                    rpc_edges.push(ContractEdge {
                        from: *caller_id,
                        to: target_id,
                        kind: EdgeKind::CallsRpc,
                        metadata: Some(target_rpc.clone()),
                        confidence,
                    });
                }
            }
        }
        self.edges.extend(rpc_edges);
    }

    #[inline]
    fn is_proto_file(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "proto")
    }

    /// O(1) in-memory reverse dependency resolution. `target` may be either a
    /// literal import-path/package identifier (as it appears in another
    /// file's `import`/`use` statement) or a declared contract symbol name
    /// (e.g. a `GrpcService`'s name) — the latter is bridged to the package(s)
    /// it's declared in before falling back to a raw substring scan.
    pub fn find_dependents(&self, target: &str) -> Vec<&ContractNode> {
        let mut result = Vec::new();
        let target_key = CompactStr::new(target);

        // 1. Direct match in reverse_deps: `target` is itself the literal
        // import-path/package string other files reference.
        if let Some(node_ids) = self.reverse_deps.get(&target_key) {
            for &id in node_ids {
                if let Some(node) = self.nodes.get(&id) {
                    result.push(node);
                }
            }
        }

        // 2. Symbol bridge: `target` may instead be a declared contract name
        // (what the tool's own schema example — "Target contract name (ex:
        // 'UserAuthRequest')" — invites callers to pass) rather than the raw
        // import string. Resolve it to its declaring node(s)' own package
        // identity, then re-query reverse_deps with THAT — a declaring node
        // always knows its own package even when nothing calls it by that
        // exact literal string anywhere else.
        if result.is_empty() {
            let mut seen_packages: HashSet<&CompactStr> = HashSet::new();
            let declaring_ids = self
                .name_to_nodes
                .get(&target_key)
                .into_iter()
                .flatten()
                .chain(self.fqcn_to_node.get(&target_key).into_iter().flatten());
            for &id in declaring_ids {
                let Some(node) = self.nodes.get(&id) else {
                    continue;
                };
                if node.package.is_empty() || !seen_packages.insert(&node.package) {
                    continue;
                }
                if let Some(node_ids) = self.reverse_deps.get(&node.package) {
                    for &dep_id in node_ids {
                        if let Some(dep_node) = self.nodes.get(&dep_id) {
                            result.push(dep_node);
                        }
                    }
                }
            }
        }

        // 2b. Graph-edge bridge: a cross-service RPC caller is linked by a
        // real `CallsRpc`/`Implements` edge (built in `reconcile_edges`), not
        // by an import string at all — a Go client, for instance, constructs
        // `pb.NewFooServiceClient(conn)` and never imports anything literally
        // named "FooService". Resolve `target` to its declaring node(s) (the
        // service itself, plus every other node declared in the same
        // package — covers a proto method node living alongside its service),
        // then collect the `from` side of any edge pointing at one of them.
        if result.is_empty() {
            let mut edge_targets: HashSet<NodeId> = HashSet::new();
            let declaring_ids = self
                .name_to_nodes
                .get(&target_key)
                .into_iter()
                .flatten()
                .chain(self.fqcn_to_node.get(&target_key).into_iter().flatten());
            for &id in declaring_ids {
                edge_targets.insert(id);
                if let Some(node) = self.nodes.get(&id) {
                    if !node.package.is_empty() {
                        if let Some(sibling_ids) = self.package_to_nodes.get(&node.package) {
                            edge_targets.extend(sibling_ids.iter().copied());
                        }
                    }
                }
            }
            let mut seen_ids: HashSet<NodeId> = HashSet::new();
            for edge in &self.edges {
                if !matches!(edge.kind, EdgeKind::CallsRpc | EdgeKind::Implements) {
                    continue;
                }
                if !edge_targets.contains(&edge.to) {
                    continue;
                }
                if let Some(caller) = self.nodes.get(&edge.from) {
                    if seen_ids.insert(caller.id) {
                        result.push(caller);
                    }
                }
            }
        }

        // 3. Last-resort fallback: unscoped substring match across every
        // recorded dependency string. Can span multiple, unrelated services
        // that happen to share a locally-aliased package name (e.g. every
        // service in a polyglot monorepo vendoring its own `genproto`
        // package) — callers should group results by `ContractNode::repo_id`
        // before presenting this to a human/agent rather than treat it as a
        // single flat, disambiguated answer.
        if result.is_empty() {
            // `reverse_deps` is a `HashMap`: iterated directly, its per-process
            // random hash seed would make this fallback return a different
            // node order on every run for identical input, violating I5.
            // Sorted by key first, so the order depends only on content.
            let mut matching_pkgs: Vec<&CompactStr> = self
                .reverse_deps
                .keys()
                .filter(|pkg| pkg.contains(target))
                .collect();
            matching_pkgs.sort();
            for pkg in matching_pkgs {
                for &id in &self.reverse_deps[pkg] {
                    if let Some(node) = self.nodes.get(&id) {
                        result.push(node);
                    }
                }
            }
        }

        result
    }

    /// Synchronous end-to-end gRPC trace resolution
    pub fn analyze_grpc(&self, target: &str) -> GrpcTrace<'_> {
        let norm_target = target.trim();
        let mut proto_definition = None;
        // Falls back to the exact-name-matched service/method node itself
        // when no `.proto` declaration is indexed (e.g. `proto_dirs` isn't
        // configured, or the workspace root simply wasn't crawled) — a
        // language's own service-implementation node (Go's
        // `RegisterFooServiceServer`-detected node, TS's `@GrpcService`
        // class, ...) is a perfectly good anchor for resolving callers via
        let mut anchors: Vec<NodeId> = Vec::new();
        let mut client_stubs: Vec<(&ContractNode, EdgeConfidence)> = Vec::new();
        let mut server_handlers: Vec<(&ContractNode, EdgeConfidence)> = Vec::new();

        for node in self.nodes.values() {
            if !matches!(node.kind, NodeKind::GrpcService | NodeKind::GrpcMethod) {
                continue;
            }
            // Case-sensitive exact-name equality is unambiguous; case-folding
            // or a substring FQCN hit is a name heuristic.
            let exact_name = node.name.as_str() == norm_target;
            let matches_name = exact_name
                || node.name.eq_ignore_ascii_case(norm_target)
                || Self::bare_names_match(&node.name, norm_target);
            let matches_fqcn = node.package.contains(norm_target)
                || Self::fqcn_contains(&node.package, &node.name, norm_target);
            if !(matches_name || matches_fqcn) {
                continue;
            }
            let confidence = if exact_name {
                EdgeConfidence::Exact
            } else {
                EdgeConfidence::Heuristic
            };

            let is_symbol_match = exact_name || Self::bare_names_match(&node.name, norm_target);

            if Self::is_proto_file(&node.file_path) {
                if exact_name || proto_definition.is_none() {
                    proto_definition = Some(node);
                }
                anchors.push(node.id);
            } else if is_symbol_match {
                anchors.push(node.id);
                // Exact or bare-name match: this node represents the target declaration/implementation.
                // It defaults straight to server_handlers without relying on directory naming heuristics.
                if node.name.as_str().starts_with("rpc:") {
                    client_stubs.push((node, confidence));
                } else {
                    server_handlers.push((node, confidence));
                }
            }
        }

        for anchor_id in &anchors {
            for edge in &self.edges {
                if edge.to != *anchor_id {
                    continue;
                }
                match edge.kind {
                    EdgeKind::Implements => {
                        if let Some(handler) = self.nodes.get(&edge.from) {
                            if !server_handlers.iter().any(|(h, _)| h.id == handler.id) {
                                server_handlers.push((handler, edge.confidence));
                            }
                        }
                    }
                    EdgeKind::CallsRpc => {
                        if let Some(client) = self.nodes.get(&edge.from) {
                            if !client_stubs.iter().any(|(c, _)| c.id == client.id) {
                                client_stubs.push((client, edge.confidence));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        GrpcTrace {
            target: CompactStr::new(norm_target),
            proto_definition,
            client_stubs,
            server_handlers,
        }
    }

    /// `format!("{package}/{name}").contains(needle)` without the allocation.
    fn fqcn_contains(package: &str, name: &str, needle: &str) -> bool {
        if package.contains(needle) || name.contains(needle) {
            return true;
        }
        // Needle may straddle the '/' separator.
        let Some((head, tail)) = needle.split_once('/') else {
            return false;
        };
        package.ends_with(head) && name.starts_with(tail)
    }

    /// Asynchronous causal impact flow resolution
    pub fn analyze_impact(&self, target: &str) -> ImpactFlow<'_> {
        let norm_target = target.trim().to_lowercase();
        let mut upstream_producers = Vec::new();
        let mut topics = Vec::new();
        let mut downstream_consumers = Vec::new();
        let mut related_sagas = Vec::new();

        // 1. Check topic registry. `topic_producers`/`topic_consumers` are
        // `HashMap`s: iterated directly, their per-process random hash seed
        // would reorder `upstream_producers`/`downstream_consumers` on every
        // run for identical input, violating I5. Sorted by topic name first.
        let mut producer_topics: Vec<&CompactStr> = self.topic_producers.keys().collect();
        producer_topics.sort();
        for topic_name in producer_topics {
            if topic_name.contains(norm_target.as_str()) {
                upstream_producers.extend(
                    self.topic_producers[topic_name]
                        .iter()
                        .filter_map(|id| self.nodes.get(id)),
                );
            }
        }

        let mut consumer_topics: Vec<&CompactStr> = self.topic_consumers.keys().collect();
        consumer_topics.sort();
        for topic_name in consumer_topics {
            if topic_name.contains(norm_target.as_str()) {
                downstream_consumers.extend(
                    self.topic_consumers[topic_name]
                        .iter()
                        .filter_map(|id| self.nodes.get(id)),
                );
            }
        }

        // 2. Check nodes for topics and sagas
        for node in self.nodes.values() {
            let interesting = matches!(
                node.kind,
                NodeKind::KafkaTopic
                    | NodeKind::EventStream
                    | NodeKind::Queue
                    | NodeKind::Saga
                    | NodeKind::PostProcessor
            );
            if !interesting || !contains_ignore_ascii_case(node.name.as_str(), &norm_target) {
                continue;
            }
            match node.kind {
                NodeKind::KafkaTopic | NodeKind::EventStream | NodeKind::Queue => topics.push(node),
                NodeKind::Saga => related_sagas.push(node),
                _ => downstream_consumers.push(node),
            }
        }

        ImpactFlow {
            target: CompactStr::new(target),
            upstream_producers,
            topics,
            downstream_consumers,
            related_sagas,
        }
    }

    /// Same causal blast-radius report as [`Self::analyze_impact`], extended with
    /// real graph traversal (BFS over `Produces`/`Consumes` edges, not another
    /// substring pass) for every hop past the first: a transitive consumer that
    /// itself produces onto another topic pulls in *that* topic's consumers too,
    /// up to `depth` hops. `depth <= 1` is exactly `analyze_impact`'s direct-only
    /// result, unchanged.
    ///
    /// `visited_topics`/`visited_nodes` bound the walk against cycles (a saga that
    /// re-produces onto a topic upstream of it) — each node and topic is expanded
    /// at most once, so the traversal always terminates even on a graph with a
    /// causal loop, matching the cycle-detection requirement for bounded C-FFI/AST
    /// walks elsewhere in this crate (see `AstGuard`).
    pub fn analyze_impact_with_depth(&self, target: &str, depth: usize) -> ImpactFlow<'_> {
        let mut flow = self.analyze_impact(target);
        let depth = depth.clamp(1, Self::MAX_IMPACT_DEPTH);
        if depth <= 1 {
            return flow;
        }

        let mut visited_topics: HashSet<NodeId> = flow.topics.iter().map(|n| n.id).collect();
        let mut visited_nodes: HashSet<NodeId> = flow
            .downstream_consumers
            .iter()
            .chain(flow.related_sagas.iter())
            .map(|n| n.id)
            .collect();
        let mut frontier: HashSet<NodeId> = visited_nodes.clone();

        for _hop in 2..=depth {
            if frontier.is_empty() {
                break;
            }

            // Every topic a node in the current frontier produces onto, in
            // stable edge-insertion order (not `HashSet` order) so the result
            // stays deterministic across runs (I5). A single pass over
            // `self.edges` — not one pass per frontier node — keeps one hop
            // O(|edges|) regardless of how wide the frontier is.
            let mut new_topics: Vec<NodeId> = Vec::new();
            let mut new_topics_set: HashSet<NodeId> = HashSet::new();
            for edge in &self.edges {
                if edge.kind == EdgeKind::Produces
                    && frontier.contains(&edge.from)
                    && visited_topics.insert(edge.to)
                {
                    new_topics.push(edge.to);
                    new_topics_set.insert(edge.to);
                }
            }
            for topic_id in &new_topics {
                if let Some(topic_node) = self.nodes.get(topic_id) {
                    flow.topics.push(topic_node);
                }
            }

            // Consumers of *any* newly-discovered topic, again in one pass
            // over `self.edges` rather than one pass per topic — keeps this
            // hop O(|edges|) too instead of O(new_topics × |edges|).
            let mut next_frontier: HashSet<NodeId> = HashSet::new();
            for edge in &self.edges {
                if edge.kind != EdgeKind::Consumes
                    || !new_topics_set.contains(&edge.to)
                    || !visited_nodes.insert(edge.from)
                {
                    continue;
                }
                let Some(consumer) = self.nodes.get(&edge.from) else {
                    continue;
                };
                next_frontier.insert(edge.from);
                match consumer.kind {
                    NodeKind::Saga => flow.related_sagas.push(consumer),
                    _ => flow.downstream_consumers.push(consumer),
                }
            }
            frontier = next_frontier;
        }

        flow
    }

    /// Case-insensitive substring search over symbol declarations, restricted to
    /// files under `scope_filter` when given. Exact-name hits come first.
    pub fn search_symbols(&self, query: &str, scope_filter: Option<&Path>) -> Vec<&ContractNode> {
        let mut matches: Vec<&ContractNode> = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();

        // Fast path: exact symbol name via the O(1) index.
        if let Some(ids) = self.name_to_nodes.get(query) {
            for id in ids {
                if let Some(node) = self.nodes.get(id) {
                    if scope_filter.is_none_or(|s| node.file_path.starts_with(s))
                        && seen.insert(node.id)
                    {
                        matches.push(node);
                    }
                }
            }
        }

        // Substring path: walk only the files inside the scope instead of every node.
        // `file_to_nodes` is a `HashMap`: sorted by path first, so this walk's
        // order depends only on content, not the per-process hash seed (I5).
        let mut in_scope_paths: Vec<&FilePath> = self
            .file_to_nodes
            .keys()
            .filter(|path| scope_filter.is_none_or(|s| path.starts_with(s)))
            .collect();
        in_scope_paths.sort();
        let in_scope = in_scope_paths
            .into_iter()
            .flat_map(|path| self.file_to_nodes[path].iter());
        for id in in_scope {
            if seen.contains(id) {
                continue;
            }
            if let Some(node) = self.nodes.get(id) {
                if contains_ignore_ascii_case(node.name.as_str(), query)
                    || Self::bare_names_match(node.name.as_str(), query)
                {
                    seen.insert(node.id);
                    matches.push(node);
                }
            }
        }

        matches
    }
}

/// Allocation-free `haystack.to_lowercase().contains(&needle.to_lowercase())` for ASCII needles.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

impl ContractGraph {
    /// Compares two bare names with normalization (case-insensitive and ignoring `_` underscores).
    /// e.g. "USER_SERVICE_NAME.SIGN_UP" or "SIGN_UP" vs "SignUp" -> true.
    pub fn bare_names_match(a: &str, b: &str) -> bool {
        let a_bare = a.split('.').next_back().unwrap_or(a);
        let b_bare = b.split('.').next_back().unwrap_or(b);

        if a_bare.eq_ignore_ascii_case(b_bare) {
            return true;
        }

        // Allocation-free: `search_symbols` runs this for every in-scope node on
        // every query. `_` is ASCII, so skipping it byte-wise never splits a char.
        let mut a_norm = a_bare.bytes().filter(|b| *b != b'_');
        let mut b_norm = b_bare.bytes().filter(|b| *b != b'_');
        let mut non_empty = false;
        loop {
            match (a_norm.next(), b_norm.next()) {
                (None, None) => return non_empty,
                (Some(x), Some(y)) if x.eq_ignore_ascii_case(&y) => non_empty = true,
                _ => return false,
            }
        }
    }

    /// Allocation-free check whether `sig` contains `bare` prefixed by `@`, `'`, `"`, `fn `, `func `, or `def `.
    fn signature_contains_bare(sig: &str, bare: &str) -> bool {
        if bare.is_empty() {
            return false;
        }
        let prefixes = ["@", "'", "\"", "fn ", "func ", "def "];
        for prefix in prefixes {
            let mut offset = 0;
            while let Some(pos) = sig[offset..].find(prefix) {
                let start = offset + pos + prefix.len();
                let rest = &sig[start..];
                if rest.starts_with(bare) {
                    return true;
                }
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !ident.is_empty() && Self::bare_names_match(&ident, bare) {
                    return true;
                }
                offset += pos + prefix.len();
            }
        }
        false
    }
}

impl ContractGraph {
    /// Canonical, `NodeId`-independent form of the graph, used to compare two
    /// builds: one JSON line per node, edge and recorded relation intent, the
    /// whole list sorted. The same facts give the same lines whatever the
    /// insertion order, thread count or id numbering; duplicates are kept so a
    /// file indexed twice stays visible.
    pub fn canonical_lines(&self) -> Vec<String> {
        let key_of = |id: NodeId| -> String {
            self.nodes
                .get(&id)
                .map_or_else(|| String::from("<missing>"), Self::canonical_node_key)
        };

        let mut lines = Vec::with_capacity(
            self.nodes.len() + self.edges.len() + self.rpc_calls.len() + self.reverse_deps.len(),
        );
        lines.extend(
            self.nodes
                .values()
                .map(|n| format!("node {}", Self::canonical_node_key(n))),
        );
        for edge in &self.edges {
            let line = serde_json::json!([
                edge.kind,
                key_of(edge.from),
                key_of(edge.to),
                edge.metadata,
                edge.confidence
            ]);
            lines.push(format!("edge {line}"));
        }
        for (label, index) in [
            ("dep", &self.reverse_deps),
            ("produces", &self.topic_producers),
            ("consumes", &self.topic_consumers),
        ] {
            for (target, ids) in index {
                for &id in ids {
                    lines.push(format!(
                        "{label} {}",
                        serde_json::json!([target, key_of(id)])
                    ));
                }
            }
        }
        for (caller, target) in &self.rpc_calls {
            lines.push(format!(
                "rpc_call {}",
                serde_json::json!([key_of(*caller), target])
            ));
        }

        lines.sort_unstable();
        lines
    }

    /// SHA-256 (hex) of [`Self::canonical_lines`].
    pub fn fingerprint(&self) -> String {
        crate::audit::AuditLogger::compute_sha256(self.canonical_lines().join("\n").as_bytes())
    }

    /// Every observable field of a node except its `NodeId`.
    fn canonical_node_key(node: &ContractNode) -> String {
        serde_json::json!([
            node.file_path.to_string_lossy(),
            node.line_start,
            node.line_end,
            node.kind,
            node.name,
            node.package,
            node.repo_id,
            node.signature,
            node.docstring
        ])
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_reverse_dependency_graph() {
        let mut graph = ContractGraph::new();
        let consumer_node = ContractNode {
            id: 0,
            name: CompactStr::new("AuthConsumer"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/auth/AuthConsumer.java").into(),
            line_start: 10,
            line_end: 50,
            package: CompactStr::new("com.mesh.auth"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        let cid = graph.add_node(consumer_node);
        graph.add_dependency(cid, "UserAuthRequest");

        let dependents = graph.find_dependents("UserAuthRequest");
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].name.as_str(), "AuthConsumer");
    }

    #[test]
    fn test_analyze_grpc_flow() {
        let mut graph = ContractGraph::new();
        let proto_node = ContractNode {
            id: 0,
            name: CompactStr::new("AuthenticateUser"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("proto-registry/auth.proto").into(),
            line_start: 20,
            line_end: 25,
            package: CompactStr::new("auth.v1"),
            repo_id: 0,
            signature: Some(CompactStr::new(
                "rpc AuthenticateUser (AuthRequest) returns (AuthResponse);",
            )),
            docstring: None,
        };
        graph.add_node(proto_node);

        let trace = graph.analyze_grpc("AuthenticateUser");
        assert!(trace.proto_definition.is_some());
        assert_eq!(
            trace.proto_definition.unwrap().name.as_str(),
            "AuthenticateUser"
        );
    }

    /// A node that only matches the target via a substring hit inside its own
    /// name (with no exact match and no real Implements edge to a proto definition)
    /// must not be bucketed into server_handlers.
    #[test]
    fn analyze_grpc_does_not_bucket_heuristic_name_matches_by_path_alone() {
        let mut graph = ContractGraph::new();
        let vendored_stub = ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutServiceServicer"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("src/emailservice/demo_pb2_grpc.py").into(),
            line_start: 709,
            line_end: 715,
            package: CompactStr::new("emailservice"),
            repo_id: 2,
            signature: None,
            docstring: None,
        };
        graph.add_node(vendored_stub);

        let trace = graph.analyze_grpc("CheckoutService");
        assert!(
            trace
                .server_handlers
                .iter()
                .all(|(h, _)| h.name.as_str() != "CheckoutServiceServicer"),
            "a heuristic substring-only name match with no exact match and \
             no Implements edge must not appear as a server handler, got: {:?}",
            trace.server_handlers
        );
    }

    /// An exact-name-matched GrpcService node is bucketed as a server
    /// handler regardless of file path naming.
    #[test]
    fn analyze_grpc_buckets_exact_match_as_server_handler_regardless_of_path_naming() {
        let mut graph = ContractGraph::new();
        let service_node = ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutService"),
            kind: NodeKind::GrpcService,
            // Deliberately no "service"/"handler"/"controller" substring
            // anywhere in this path.
            file_path: Path::new("src/checkout/main.go").into(),
            line_start: 142,
            line_end: 142,
            package: CompactStr::new("checkout"),
            repo_id: 0,
            signature: None,
            docstring: None,
        };
        graph.add_node(service_node);

        let trace = graph.analyze_grpc("CheckoutService");
        assert!(
            trace
                .server_handlers
                .iter()
                .any(|(h, _)| h.name.as_str() == "CheckoutService"),
            "an exact-name GrpcService match must be a server handler \
             regardless of directory naming, got server_handlers={:?} \
             client_stubs={:?}",
            trace.server_handlers,
            trace.client_stubs
        );
        assert!(
            trace
                .client_stubs
                .iter()
                .all(|(c, _)| c.name.as_str() != "CheckoutService"),
            "must not also appear in client_stubs"
        );
    }

    /// Regression test: a synthetic node created by the generic
    /// custom-pattern engine's `PatternKind::Rpc` (name prefixed `"rpc:"`)
    /// explicitly represents a client call site, not a declaration — it must
    /// still route to client_stubs, not server_handlers, even under an
    /// exact-name match.
    #[test]
    fn analyze_grpc_routes_custom_pattern_rpc_node_to_client_stubs() {
        let mut graph = ContractGraph::new();
        let rpc_call_node = ContractNode {
            id: 0,
            name: CompactStr::new("rpc:CheckoutService"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("src/some-service/handler.ts").into(),
            line_start: 10,
            line_end: 12,
            package: CompactStr::new("some-service"),
            repo_id: 0,
            signature: Some(CompactStr::new("RPC call: CheckoutService")),
            docstring: None,
        };
        graph.add_node(rpc_call_node);

        let trace = graph.analyze_grpc("rpc:CheckoutService");
        assert!(
            trace
                .client_stubs
                .iter()
                .any(|(c, _)| c.name.as_str() == "rpc:CheckoutService"),
            "a custom-pattern RPC call-site node must be a client stub, got: {:?}",
            trace
        );
        assert!(trace
            .server_handlers
            .iter()
            .all(|(h, _)| h.name.as_str() != "rpc:CheckoutService"));
    }

    /// Regression test: `analyze_grpc`'s client/server edge resolution must
    /// not require a `.proto` file to be indexed at all — a language's own
    /// exact-name service node (e.g. Go's `RegisterFooServiceServer`-detected
    /// node) is a perfectly good anchor for a real `CallsRpc` edge.
    #[test]
    fn analyze_grpc_resolves_client_stubs_via_edge_without_a_proto_file() {
        let mut graph = ContractGraph::new();

        let service_node = ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("src/checkoutservice/main.go").into(),
            line_start: 142,
            line_end: 142,
            package: CompactStr::new("checkoutservice"),
            repo_id: 0,
            signature: None,
            docstring: None,
        };
        let service_id = graph.add_node(service_node);

        let caller_node = ContractNode {
            id: 0,
            name: CompactStr::new("placeOrderHandler"),
            kind: NodeKind::HttpEndpoint,
            file_path: Path::new("src/frontend/handlers.go").into(),
            line_start: 320,
            line_end: 401,
            package: CompactStr::new("frontend"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        let caller_id = graph.add_node(caller_node);
        graph.add_rpc_call(caller_id, "CheckoutService");
        graph.reconcile_edges();

        let trace = graph.analyze_grpc("CheckoutService");
        assert!(trace.proto_definition.is_none());
        assert_eq!(
            trace
                .server_handlers
                .iter()
                .find(|(h, _)| h.id == service_id)
                .map(|_| ()),
            Some(()),
            "the service node itself must still be its own server handler entry"
        );
        assert!(
            trace
                .client_stubs
                .iter()
                .any(|(c, _)| c.name.as_str() == "placeOrderHandler"),
            "expected the real CallsRpc caller to appear in client_stubs even \
             with no .proto file indexed, got: {:?}",
            trace.client_stubs
        );
    }

    /// Resolves a cross-service gRPC caller via CallsRpc edge: client construction
    /// links caller to GrpcService through reconcile_edges.
    #[test]
    fn find_dependents_resolves_a_real_cross_service_grpc_caller_via_reconcile_edges() {
        let mut graph = ContractGraph::new();

        let service_node = ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("src/checkoutservice/main.go").into(),
            line_start: 142,
            line_end: 142,
            package: CompactStr::new("checkoutservice"),
            repo_id: 0,
            signature: None,
            docstring: None,
        };
        graph.add_node(service_node);

        let caller_node = ContractNode {
            id: 0,
            name: CompactStr::new("placeOrderHandler"),
            kind: NodeKind::HttpEndpoint,
            file_path: Path::new("src/frontend/handlers.go").into(),
            line_start: 320,
            line_end: 401,
            package: CompactStr::new("frontend"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        let caller_id = graph.add_node(caller_node);

        // The raw signal a language extractor records at a
        // `pb.NewCheckoutServiceClient(conn)` call site — no import-path
        // relationship involved at all.
        graph.add_rpc_call(caller_id, "CheckoutService");
        graph.reconcile_edges();

        let dependents = graph.find_dependents("CheckoutService");
        assert_eq!(
            dependents.len(),
            1,
            "a real CallsRpc-edge caller must resolve even though no file \
             imports anything named 'CheckoutService', got: {dependents:?}"
        );
        assert_eq!(dependents[0].name.as_str(), "placeOrderHandler");
    }

    /// Regression test for querying `find_dependents` by a declared symbol's
    /// own name (a GrpcService, the exact shape the tool's own schema
    /// example invites — "Target contract name (ex: 'UserAuthRequest')")
    /// which must not silently return nothing just because no other file
    /// happens to reference that literal string — `frontend` depends on
    /// `checkoutservice`'s *package*, not on the string "CheckoutService".
    #[test]
    fn find_dependents_resolves_by_declared_symbol_name() {
        let mut graph = ContractGraph::new();
        let service_node = ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("src/checkoutservice/main.go").into(),
            line_start: 142,
            line_end: 142,
            package: CompactStr::new("checkoutservice"),
            repo_id: 0,
            signature: None,
            docstring: None,
        };
        graph.add_node(service_node);

        let caller_node = ContractNode {
            id: 0,
            name: CompactStr::new("placeOrderHandler"),
            kind: NodeKind::HttpEndpoint,
            file_path: Path::new("src/frontend/handlers.go").into(),
            line_start: 320,
            line_end: 401,
            package: CompactStr::new("frontend"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        let caller_id = graph.add_node(caller_node);
        // The caller's recorded dependency is on the *package*, never the
        // literal gRPC service name.
        graph.add_dependency(caller_id, "checkoutservice");

        let dependents = graph.find_dependents("CheckoutService");
        assert_eq!(
            dependents.len(),
            1,
            "querying by the declared symbol name must resolve via its \
             package, not return an empty (false-negative) result"
        );
        assert_eq!(dependents[0].name.as_str(), "placeOrderHandler");
    }

    /// Regression test for the second half of the same finding: once results
    /// span multiple services that happen to share a locally-aliased package
    /// name (every service vendoring its own `genproto`), each result still
    /// carries enough info (`repo_id`) for a caller to group/disambiguate
    /// them, rather than a single flattened, unscoped list.
    #[test]
    fn find_dependents_substring_fallback_preserves_repo_id_for_grouping() {
        let mut graph = ContractGraph::new();

        let svc_a_caller = ContractNode {
            id: 0,
            name: CompactStr::new("loadCatalog"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("src/productcatalogservice/catalog_loader.go").into(),
            line_start: 33,
            line_end: 42,
            package: CompactStr::new("productcatalogservice"),
            repo_id: 0,
            signature: None,
            docstring: None,
        };
        let a_id = graph.add_node(svc_a_caller);
        graph.add_dependency(a_id, "genproto");

        let svc_b_caller = ContractNode {
            id: 0,
            name: CompactStr::new("placeOrderHandler"),
            kind: NodeKind::HttpEndpoint,
            file_path: Path::new("src/frontend/handlers.go").into(),
            line_start: 320,
            line_end: 401,
            package: CompactStr::new("frontend"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        let b_id = graph.add_node(svc_b_caller);
        graph.add_dependency(b_id, "genproto");

        let dependents = graph.find_dependents("genproto");
        assert_eq!(dependents.len(), 2);
        let repo_ids: HashSet<RepoId> = dependents.iter().map(|n| n.repo_id).collect();
        assert_eq!(
            repo_ids.len(),
            2,
            "results from unrelated services sharing a locally-aliased \
             package name must retain distinct repo_id so a caller can \
             group them instead of treating this as one disambiguated match"
        );
    }

    /// The substring fallback in `find_dependents` (step 3, no exact/symbol/edge
    /// match found) used to iterate `reverse_deps` — a `HashMap` — directly,
    /// so the returned node order depended on the process's random hash seed
    /// instead of on the workspace's content (idempotence invariant I5). It
    /// must come back sorted by the matched package name, deterministically,
    /// regardless of node/registration order.
    #[test]
    fn find_dependents_substring_fallback_is_ordered_by_matched_package() {
        let mut graph = ContractGraph::new();
        // Registered in an order that does not match the expected (sorted)
        // output order, so a HashMap-order regression would be caught.
        for (pkg, name, path) in [
            ("zzz-genproto-shared", "zCaller", "z.go"),
            ("aaa-genproto-shared", "aCaller", "a.go"),
            ("mmm-genproto-shared", "mCaller", "m.go"),
        ] {
            let id = graph.add_node(ContractNode {
                id: 0,
                name: CompactStr::new(name),
                kind: NodeKind::ServiceClass,
                file_path: Path::new(path).into(),
                line_start: 1,
                line_end: 1,
                package: CompactStr::new(pkg),
                repo_id: 0,
                signature: None,
                docstring: None,
            });
            graph.add_dependency(id, pkg);
        }

        let dependents = graph.find_dependents("genproto");
        let packages: Vec<&str> = dependents.iter().map(|n| n.package.as_str()).collect();
        assert_eq!(
            packages,
            vec![
                "aaa-genproto-shared",
                "mmm-genproto-shared",
                "zzz-genproto-shared"
            ],
            "substring fallback must be sorted by matched package name, not HashMap order"
        );
    }

    /// `analyze_impact`'s topic-registry pass used to iterate `topic_producers`/
    /// `topic_consumers` — both `HashMap`s — directly, so which producer/
    /// consumer topic matched first (and thus its node order in the result)
    /// depended on the process's random hash seed rather than content (I5).
    #[test]
    fn analyze_impact_orders_producers_by_matched_topic_name() {
        let mut graph = ContractGraph::new();
        for (topic, name, path) in [
            ("orders.zzz", "zProducer", "z.go"),
            ("orders.aaa", "aProducer", "a.go"),
            ("orders.mmm", "mProducer", "m.go"),
        ] {
            let id = graph.add_node(ContractNode {
                id: 0,
                name: CompactStr::new(name),
                kind: NodeKind::ServiceClass,
                file_path: Path::new(path).into(),
                line_start: 1,
                line_end: 1,
                package: CompactStr::new("shop"),
                repo_id: 0,
                signature: None,
                docstring: None,
            });
            graph.add_producer(id, topic);
        }

        let flow = graph.analyze_impact("orders");
        let names: Vec<&str> = flow
            .upstream_producers
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["aProducer", "mProducer", "zProducer"],
            "producers must be ordered by their matched topic name, not HashMap order"
        );
    }

    /// Builds `orders.created` -[Consumes]-> `billingHandler` -[Produces]-> `payment.settled`
    /// -[Consumes]-> `ledgerHandler`, so a depth-1 query only ever sees `billingHandler`
    /// while depth-2 pulls in `payment.settled` and `ledgerHandler` too — proving the
    /// traversal follows real edges rather than re-running the substring pass.
    #[test]
    fn analyze_impact_with_depth_follows_produces_consumes_edges_transitively() {
        let mut graph = ContractGraph::new();
        let topic_orders = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("orders.created"),
            kind: NodeKind::KafkaTopic,
            file_path: Path::new("infra/topics.yaml").into(),
            line_start: 1,
            line_end: 1,
            package: CompactStr::new(""),
            repo_id: 0,
            signature: None,
            docstring: None,
        });
        let billing_handler = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("billingHandler"),
            kind: NodeKind::PostProcessor,
            file_path: Path::new("services/billing/handler.go").into(),
            line_start: 10,
            line_end: 20,
            package: CompactStr::new("billing"),
            repo_id: 1,
            signature: None,
            docstring: None,
        });
        let topic_settled = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("payment.settled"),
            kind: NodeKind::KafkaTopic,
            file_path: Path::new("infra/topics.yaml").into(),
            line_start: 2,
            line_end: 2,
            package: CompactStr::new(""),
            repo_id: 0,
            signature: None,
            docstring: None,
        });
        let ledger_handler = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("ledgerHandler"),
            kind: NodeKind::PostProcessor,
            file_path: Path::new("services/ledger/handler.go").into(),
            line_start: 5,
            line_end: 15,
            package: CompactStr::new("ledger"),
            repo_id: 2,
            signature: None,
            docstring: None,
        });

        graph.add_edge(ContractEdge {
            from: billing_handler,
            to: topic_orders,
            kind: EdgeKind::Consumes,
            metadata: None,
            confidence: EdgeConfidence::Exact,
        });
        graph.add_edge(ContractEdge {
            from: billing_handler,
            to: topic_settled,
            kind: EdgeKind::Produces,
            metadata: None,
            confidence: EdgeConfidence::Exact,
        });
        graph.add_edge(ContractEdge {
            from: ledger_handler,
            to: topic_settled,
            kind: EdgeKind::Consumes,
            metadata: None,
            confidence: EdgeConfidence::Exact,
        });
        // Cycle: the ledger handler re-produces onto the original topic. Without
        // visited-set cycle detection this would loop forever re-discovering
        // `billingHandler`/`topic_settled` at every hop.
        graph.add_edge(ContractEdge {
            from: ledger_handler,
            to: topic_orders,
            kind: EdgeKind::Produces,
            metadata: None,
            confidence: EdgeConfidence::Exact,
        });

        let direct = graph.analyze_impact_with_depth("orders.created", 1);
        let direct_names: HashSet<&str> = direct
            .downstream_consumers
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(
            direct_names,
            HashSet::from(["billingHandler"]),
            "depth 1 must match plain analyze_impact: direct consumers only"
        );
        assert!(
            direct
                .topics
                .iter()
                .all(|t| t.name.as_str() != "payment.settled"),
            "depth 1 must not pull in a topic two hops away"
        );

        let transitive = graph.analyze_impact_with_depth("orders.created", 2);
        let transitive_names: HashSet<&str> = transitive
            .downstream_consumers
            .iter()
            .map(|n| n.name.as_str())
            .collect();
        assert_eq!(
            transitive_names,
            HashSet::from(["billingHandler", "ledgerHandler"]),
            "depth 2 must include the consumer of the topic billingHandler produces onto"
        );
        assert!(
            transitive
                .topics
                .iter()
                .any(|t| t.name.as_str() == "payment.settled"),
            "depth 2 must surface the transitively-produced topic"
        );

        // The cycle back to `orders.created` must not re-add it as a "new"
        // downstream discovery, and higher depths must terminate rather than
        // looping forever rediscovering the same two nodes.
        let deep = graph.analyze_impact_with_depth("orders.created", 5);
        assert_eq!(
            deep.downstream_consumers.len(),
            transitive.downstream_consumers.len(),
            "cycle back to the origin topic must not manufacture duplicate consumers at higher depth"
        );

        let over_cap = graph.analyze_impact_with_depth("orders.created", 200);
        assert_eq!(
            over_cap.downstream_consumers.len(),
            deep.downstream_consumers.len(),
            "requested depth must be clamped to MAX_IMPACT_DEPTH, not iterate 200 hops"
        );
    }

    /// A bare-name import (ambiguous across packages) must not be reported with
    /// the same confidence as an exact fully-qualified import.
    #[test]
    fn test_import_resolution_confidence_distinguishes_fqcn_from_bare_name() {
        let mut graph = ContractGraph::new();

        // Two unrelated `Invoice` types in different packages — the classic
        // name-collision the roadmap calls out.
        let acme_invoice = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("Invoice"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/billing/acme/Invoice.java").into(),
            line_start: 1,
            line_end: 10,
            package: CompactStr::new("com.acme.billing"),
            repo_id: 1,
            signature: None,
            docstring: None,
        });
        graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("Invoice"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/billing/other/Invoice.java").into(),
            line_start: 1,
            line_end: 10,
            package: CompactStr::new("com.other.billing"),
            repo_id: 2,
            signature: None,
            docstring: None,
        });

        let bare_importer = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("BareImporter"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/consumer/BareImporter.java").into(),
            line_start: 1,
            line_end: 5,
            package: CompactStr::new("com.mesh.consumer"),
            repo_id: 3,
            signature: None,
            docstring: None,
        });
        let fqcn_importer = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("FqcnImporter"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/consumer/FqcnImporter.java").into(),
            line_start: 1,
            line_end: 5,
            package: CompactStr::new("com.mesh.consumer"),
            repo_id: 3,
            signature: None,
            docstring: None,
        });
        let rust_importer = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("RustImporter"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/consumer/rust_importer.rs").into(),
            line_start: 1,
            line_end: 5,
            package: CompactStr::new("com.mesh.consumer"),
            repo_id: 3,
            signature: None,
            docstring: None,
        });

        // Bare, unqualified import — ambiguous, must resolve as Heuristic.
        graph.add_dependency(bare_importer, "Invoice");
        // Fully-qualified Java-style import — unambiguous, must resolve as Exact
        // and must land specifically on the `com.acme.billing` node.
        graph.add_dependency(fqcn_importer, "com.acme.billing.Invoice");
        // Fully-qualified Rust-style path — same requirement, `::`-separated.
        graph.add_dependency(rust_importer, "com::acme::billing::Invoice");

        graph.reconcile_edges();

        let find_import_edge = |from: NodeId| {
            graph
                .all_edges()
                .iter()
                .find(|e| e.kind == EdgeKind::Imports && e.from == from)
                .expect("resolved import edge should exist")
        };

        let fqcn_edge = find_import_edge(fqcn_importer);
        let rust_edge = find_import_edge(rust_importer);

        assert_eq!(fqcn_edge.confidence, EdgeConfidence::Exact);
        assert_eq!(rust_edge.confidence, EdgeConfidence::Exact);

        // The whole point: the FQCN/`::` matches must resolve to the exact
        // node their qualifier names, not merely "a" node sharing the bare name.
        assert_eq!(fqcn_edge.to, acme_invoice);
        assert_eq!(rust_edge.to, acme_invoice);

        // The bare import is a genuine tie: neither `Invoice` candidate shares
        // `bare_importer`'s repo, so the graph must not silently pick one — it
        // must emit an edge to *both*, tagged `Ambiguous`, rather than a single
        // confident-looking `Heuristic` edge to whichever candidate happened to
        // be inserted first.
        let bare_edges: Vec<_> = graph
            .all_edges()
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports && e.from == bare_importer)
            .collect();
        assert_eq!(
            bare_edges.len(),
            2,
            "a genuine bare-name tie must fan out to every candidate, got: {bare_edges:?}"
        );
        assert!(bare_edges
            .iter()
            .all(|e| e.confidence == EdgeConfidence::Ambiguous));
        let bare_targets: std::collections::HashSet<NodeId> =
            bare_edges.iter().map(|e| e.to).collect();
        assert!(bare_targets.contains(&acme_invoice));

        // Bug-in-the-test guard: if every match ends up the same confidence, the
        // field is decorative, not signal.
        assert_ne!(bare_edges[0].confidence, fqcn_edge.confidence);
    }

    /// The `Implements` reconciliation likewise must not claim `Exact`
    /// confidence for a handler that only matches the proto method by a
    /// case-insensitive / PascalCase heuristic rather than the literal FQCN.
    #[test]
    fn test_implements_reconciliation_confidence_distinguishes_heuristic_match() {
        let mut graph = ContractGraph::new();

        let proto_method = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("auth.v1.AuthenticateUser"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("proto-registry/auth.proto").into(),
            line_start: 20,
            line_end: 25,
            package: CompactStr::new("auth.v1"),
            repo_id: 0,
            signature: Some(CompactStr::new(
                "rpc AuthenticateUser (AuthRequest) returns (AuthResponse);",
            )),
            docstring: None,
        });

        // Handler whose name is only a case-insensitive match to the bare
        // method name ("authenticateuser" vs "AuthenticateUser") — heuristic.
        let heuristic_handler = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("authenticateuser"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("services/auth/AuthController.java").into(),
            line_start: 15,
            line_end: 30,
            package: CompactStr::new("com.mesh.auth"),
            repo_id: 1,
            signature: None,
            docstring: None,
        });

        // Handler whose name is the literal FQCN — exact.
        let exact_handler = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("auth.v1.AuthenticateUser"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("services/auth/AuthServiceImpl.java").into(),
            line_start: 15,
            line_end: 30,
            package: CompactStr::new("com.mesh.auth"),
            repo_id: 1,
            signature: None,
            docstring: None,
        });
        graph.reconcile_edges();

        let implements_edges: Vec<_> = graph
            .all_edges()
            .iter()
            .filter(|e| e.kind == EdgeKind::Implements && e.to == proto_method)
            .collect();

        let heuristic_edge = implements_edges
            .iter()
            .find(|e| e.from == heuristic_handler)
            .expect("heuristic handler should still be linked");
        let exact_edge = implements_edges
            .iter()
            .find(|e| e.from == exact_handler)
            .expect("exact handler should be linked");

        assert_eq!(heuristic_edge.confidence, EdgeConfidence::Heuristic);
        assert_eq!(exact_edge.confidence, EdgeConfidence::Exact);
        assert_ne!(heuristic_edge.confidence, exact_edge.confidence);
    }

    #[test]
    fn test_large_scale_repo_indexing() {
        let mut graph = ContractGraph::new();

        // Register 500 microservices (exceeding 255 u8 limit)
        for repo_idx in 0..500u16 {
            let node = ContractNode {
                id: 0,
                name: CompactStr::new(format!("ServiceHandler{repo_idx}")),
                kind: NodeKind::ServiceClass,
                file_path: PathBuf::from(format!("services/service_{repo_idx}/Handler.go")).into(),
                line_start: 1,
                line_end: 100,
                package: CompactStr::new(format!("service.{repo_idx}")),
                repo_id: repo_idx,
                signature: None,
                docstring: None,
            };
            let nid = graph.add_node(node);
            if repo_idx % 2 == 0 {
                graph.add_dependency(nid, "SharedEnterpriseContract");
            }
        }

        assert_eq!(graph.node_count(), 500);

        let dependents = graph.find_dependents("SharedEnterpriseContract");
        assert_eq!(dependents.len(), 250);

        // Verify high repo_id (e.g. repo 498) is accurately preserved without overflow
        let high_repo_node = dependents.iter().find(|n| n.repo_id == 498);
        assert!(high_repo_node.is_some());
        assert_eq!(high_repo_node.unwrap().name.as_str(), "ServiceHandler498");
    }

    /// A hub topic with producers and consumers generates linear O(P + C)
    /// Produces/Consumes edges, rather than quadratic O(P * C) edges.
    #[test]
    fn test_hub_topic_edge_growth_is_linear_not_quadratic() {
        let mut graph = ContractGraph::new();

        const FANOUT: usize = 50;
        let mut producer_ids = Vec::with_capacity(FANOUT);
        let mut consumer_ids = Vec::with_capacity(FANOUT);

        for i in 0..FANOUT {
            let producer = ContractNode {
                id: 0,
                name: CompactStr::new(format!("Producer{i}")),
                kind: NodeKind::ServiceClass,
                file_path: Path::new(&format!("services/producer_{i}/Handler.go")).into(),
                line_start: 1,
                line_end: 10,
                package: CompactStr::new(format!("producer.{i}")),
                repo_id: i as RepoId,
                signature: None,
                docstring: None,
            };
            let pid = graph.add_node(producer);
            graph.add_producer(pid, "hub-topic");
            producer_ids.push(pid);

            let consumer = ContractNode {
                id: 0,
                name: CompactStr::new(format!("Consumer{i}")),
                kind: NodeKind::ServiceClass,
                file_path: Path::new(&format!("services/consumer_{i}/Handler.go")).into(),
                line_start: 1,
                line_end: 10,
                package: CompactStr::new(format!("consumer.{i}")),
                repo_id: (FANOUT + i) as RepoId,
                signature: None,
                docstring: None,
            };
            let cid = graph.add_node(consumer);
            graph.add_consumer(cid, "hub-topic");
            consumer_ids.push(cid);
        }

        graph.reconcile_edges();

        let produces_count = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Produces)
            .count();
        let consumes_count = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Consumes)
            .count();
        let dispatches_count = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::DispatchesTo)
            .count();

        // Bounded: one Produces edge per producer, one Consumes edge per
        // consumer, and no direct producer->consumer edges at all — never the
        // 2,500 (50*50) pairs a quadratic pass would have generated.
        assert_eq!(produces_count, FANOUT);
        assert_eq!(consumes_count, FANOUT);
        assert_eq!(dispatches_count, 0);

        let total_topic_edges = produces_count + consumes_count + dispatches_count;
        assert_eq!(total_topic_edges, FANOUT * 2);
        assert!(total_topic_edges < FANOUT * FANOUT);
    }

    #[test]
    fn test_grpc_enum_decorator_end_to_end_reconciliation() {
        let mut graph = ContractGraph::new();

        // 1. Proto RPC declaration node
        let proto_node = ContractNode {
            id: 0,
            name: CompactStr::new("volontariapp.user.v1.UserService.SignUp"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new("proto/user.proto").into(),
            line_start: 10,
            line_end: 15,
            package: CompactStr::new("volontariapp.user.v1"),
            repo_id: 1,
            signature: Some(CompactStr::new(
                "rpc SignUp(SignUpRequest) returns (SignUpResponse)",
            )),
            docstring: None,
        };
        graph.add_node(proto_node);

        // 2. TS Controller Handler node extracted from @GrpcMethod(USER_SERVICE_NAME, UserCommandMethod.SIGN_UP)
        let ts_handler = ContractNode {
            id: 0,
            name: CompactStr::new("USER_SERVICE_NAME.SIGN_UP"),
            kind: NodeKind::GrpcMethod,
            file_path: Path::new(
                "ms-user/src/modules/user/controllers/command/user.command.controller.ts",
            )
            .into(),
            line_start: 47,
            line_end: 50,
            package: CompactStr::new("ms-user"),
            repo_id: 1,
            signature: Some(CompactStr::new(
                "@GrpcMethod(USER_SERVICE_NAME, UserCommandMethod.SIGN_UP) async signUp(data: SignUpCommandDTO)",
            )),
            docstring: None,
        };
        graph.add_node(ts_handler);

        // Reconcile graph edges
        graph.reconcile_edges();

        // Query analyze_grpc("SignUp")
        let trace = graph.analyze_grpc("SignUp");
        assert!(
            trace.proto_definition.is_some(),
            "Proto definition for SignUp should be found"
        );
        assert!(
            !trace.server_handlers.is_empty(),
            "Server handlers for SignUp must not be empty"
        );
        assert_eq!(
            trace.server_handlers[0].0.name.as_str(),
            "USER_SERVICE_NAME.SIGN_UP"
        );

        // Query analyze_grpc("UserService")
        let trace_svc = graph.analyze_grpc("UserService");
        assert!(!trace_svc.server_handlers.is_empty());

        // Query search_symbols("signUp")
        let symbols = graph.search_symbols("signUp", None);
        assert!(symbols
            .iter()
            .any(|n| n.name.as_str() == "USER_SERVICE_NAME.SIGN_UP"));
    }

    #[test]
    fn test_analyze_grpc_multi_service_anchors_resolves_all_callers() {
        let mut graph = ContractGraph::new();

        // Service v1 in auth
        let svc_v1 = ContractNode {
            id: 0,
            name: CompactStr::new("AuthService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("services/auth-v1/server.go").into(),
            line_start: 10,
            line_end: 20,
            package: CompactStr::new("auth.v1"),
            repo_id: 1,
            signature: None,
            docstring: None,
        };
        // Service v2 in new auth
        let svc_v2 = ContractNode {
            id: 0,
            name: CompactStr::new("AuthService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("services/auth-v2/server.go").into(),
            line_start: 10,
            line_end: 20,
            package: CompactStr::new("auth.v2"),
            repo_id: 2,
            signature: None,
            docstring: None,
        };
        // Caller A targeting v1
        let caller_a = ContractNode {
            id: 0,
            name: CompactStr::new("callAuthV1"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/gateway/client1.go").into(),
            line_start: 5,
            line_end: 15,
            package: CompactStr::new("gateway"),
            repo_id: 3,
            signature: None,
            docstring: None,
        };
        // Caller B targeting v2
        let caller_b = ContractNode {
            id: 0,
            name: CompactStr::new("callAuthV2"),
            kind: NodeKind::ServiceClass,
            file_path: Path::new("services/billing/client2.go").into(),
            line_start: 5,
            line_end: 15,
            package: CompactStr::new("billing"),
            repo_id: 4,
            signature: None,
            docstring: None,
        };

        let id_v1 = graph.add_node(svc_v1);
        let id_v2 = graph.add_node(svc_v2);
        let id_caller_a = graph.add_node(caller_a);
        let id_caller_b = graph.add_node(caller_b);

        graph.add_edge(ContractEdge {
            from: id_caller_a,
            to: id_v1,
            kind: EdgeKind::CallsRpc,
            metadata: Some(CompactStr::new("AuthService")),
            confidence: EdgeConfidence::Exact,
        });
        graph.add_edge(ContractEdge {
            from: id_caller_b,
            to: id_v2,
            kind: EdgeKind::CallsRpc,
            metadata: Some(CompactStr::new("AuthService")),
            confidence: EdgeConfidence::Exact,
        });

        let trace = graph.analyze_grpc("AuthService");
        assert_eq!(
            trace.server_handlers.len(),
            2,
            "both v1 and v2 AuthService must be server handlers"
        );
        assert_eq!(
            trace.client_stubs.len(),
            2,
            "both callers to v1 and v2 must be resolved without anchor overwrite"
        );
        assert!(trace
            .client_stubs
            .iter()
            .any(|(c, _)| c.name.as_str() == "callAuthV1"));
        assert!(trace
            .client_stubs
            .iter()
            .any(|(c, _)| c.name.as_str() == "callAuthV2"));
    }

    fn fp_node(name: &str, file: &str, line: usize, kind: NodeKind) -> ContractNode {
        ContractNode {
            id: 0,
            name: CompactStr::new(name),
            kind,
            file_path: Path::new(file).into(),
            line_start: line,
            line_end: line + 2,
            package: CompactStr::new("pkg"),
            repo_id: 0,
            signature: Some(CompactStr::new(format!("sig {name}"))),
            docstring: None,
        }
    }

    /// The fingerprint must depend on graph content only: inserting the same
    /// nodes and relations in a different order renumbers every `NodeId` but
    /// must not change it.
    #[test]
    fn fingerprint_ignores_insertion_order_and_node_ids() {
        let build = |reversed: bool| {
            let mut graph = ContractGraph::new();
            let mut specs = vec![
                ("Widget", "a/widget.go", 1, NodeKind::ServiceClass),
                ("Use", "b/use.go", 4, NodeKind::ServiceClass),
                ("Emit", "c/emit.go", 7, NodeKind::ServiceClass),
            ];
            if reversed {
                specs.reverse();
            }
            let mut ids = HashMap::new();
            for (name, file, line, kind) in specs {
                ids.insert(name, graph.add_node(fp_node(name, file, line, kind)));
            }
            graph.add_dependency(ids["Use"], "Widget");
            graph.add_producer(ids["Emit"], "orders");
            graph.add_consumer(ids["Use"], "orders");
            graph.reconcile_edges();
            graph
        };

        let forward = build(false);
        let reversed = build(true);
        assert_eq!(forward.canonical_lines(), reversed.canonical_lines());
        assert_eq!(forward.fingerprint(), reversed.fingerprint());
    }

    /// Any content difference — here a single line number — must change it.
    #[test]
    fn fingerprint_changes_when_content_changes() {
        let mut a = ContractGraph::new();
        a.add_node(fp_node("Widget", "a/widget.go", 1, NodeKind::ServiceClass));
        let mut b = ContractGraph::new();
        b.add_node(fp_node("Widget", "a/widget.go", 2, NodeKind::ServiceClass));
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    /// A node indexed twice (e.g. through two overlapping roots) must stay
    /// visible in the canonical form rather than collapse into one line.
    #[test]
    fn canonical_lines_keep_duplicate_nodes() {
        let mut graph = ContractGraph::new();
        graph.add_node(fp_node("Widget", "a/widget.go", 1, NodeKind::ServiceClass));
        graph.add_node(fp_node("Widget", "a/widget.go", 1, NodeKind::ServiceClass));
        let node_lines = graph
            .canonical_lines()
            .into_iter()
            .filter(|l| l.starts_with("node "))
            .count();
        assert_eq!(node_lines, 2);
    }
}
