use crate::types::CompactStr;
use crate::types::{
    ContractEdge, ContractNode, EdgeConfidence, EdgeKind, FilePath, NodeId, NodeKind, RepoId,
};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
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
    nodes: HashMap<NodeId, ContractNode>,
    edges: Vec<ContractEdge>,

    // O(1) in-memory indices
    name_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    package_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    file_to_nodes: HashMap<FilePath, Vec<NodeId>>,
    fqcn_to_node: HashMap<CompactStr, NodeId>,
    reverse_deps: HashMap<CompactStr, Vec<NodeId>>,
    topic_producers: HashMap<CompactStr, Vec<NodeId>>,
    topic_consumers: HashMap<CompactStr, Vec<NodeId>>,
    rpc_calls: Vec<(NodeId, CompactStr)>,

    next_node_id: u32,
}

impl ContractGraph {
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

        if node.kind == NodeKind::GrpcMethod || node.kind == NodeKind::GrpcService {
            let fqcn = format!("{}/{}", node.package, node.name);
            self.fqcn_to_node.insert(CompactStr::new(&fqcn), id);
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

    pub fn add_dependency(&mut self, consumer_node_id: NodeId, imported_target: &str) {
        let key = CompactStr::new(imported_target);
        self.reverse_deps
            .entry(key)
            .or_default()
            .push(consumer_node_id);

        self.add_edge(ContractEdge {
            from: consumer_node_id,
            to: 0, // dynamic link resolved at query time
            kind: EdgeKind::Imports,
            metadata: Some(CompactStr::new(imported_target)),
            // Placeholder — `reconcile_edges` overwrites this with the real
            // confidence once `resolve_import_target` runs; `Heuristic` here
            // is just a conservative default in case reconciliation never runs.
            confidence: EdgeConfidence::Heuristic,
        });
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
            if node.kind == NodeKind::GrpcMethod || node.kind == NodeKind::GrpcService {
                let fqcn = CompactStr::new(format!("{}/{}", node.package, node.name));
                if self.fqcn_to_node.get(&fqcn) == Some(id) {
                    self.fqcn_to_node.remove(&fqcn);
                }
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

    /// Resolves the target of a placeholder `Imports` edge using the O(1) indices.
    ///
    /// Returns the resolved node together with the confidence of the
    /// strategy that found it (see `EdgeConfidence` / ROADMAP Item 6).
    fn resolve_import_target(
        &self,
        importer: NodeId,
        target_str: &str,
    ) -> Option<(NodeId, EdgeConfidence)> {
        let is_relative_or_absolute_path =
            target_str.starts_with('.') || target_str.starts_with('/');

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
                        return Some((id, EdgeConfidence::Exact));
                    }
                }
                let fqcn_key = format!("{pkg}/{name}");
                if let Some(&id) = self.fqcn_to_node.get(fqcn_key.as_str()) {
                    return Some((id, EdgeConfidence::Exact));
                }
            }
        }

        // 2. Exact symbol name anywhere in the mesh — a bare name, so two
        // unrelated types sharing it would collide; heuristic.
        if let Some(ids) = self.name_to_nodes.get(target_str) {
            if let Some(&id) = ids.first() {
                return Some((id, EdgeConfidence::Heuristic));
            }
        }

        let is_qualified = target_str.contains('/') || target_str.contains('.');

        // 3. Relative import: match on file stem, same repo as the importer.
        if is_relative_or_absolute_path {
            let target_stem = Path::new(target_str)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(target_str);
            let importer_repo = self.nodes.get(&importer).map(|n| n.repo_id);
            if let Some(ids) = self.name_to_nodes.get(target_stem) {
                if let Some(&id) = ids
                    .iter()
                    .find(|id| self.nodes.get(id).map(|n| n.repo_id) == importer_repo)
                {
                    return Some((id, EdgeConfidence::Heuristic));
                }
            }
        }

        // 4. Qualified package identifier (e.g. '@scope/pkg', 'com.acme.billing')
        // matched against a whole package, not a specific symbol; heuristic.
        if is_qualified && !is_relative_or_absolute_path {
            if let Some(ids) = self.package_to_nodes.get(target_str) {
                if let Some(&id) = ids.first() {
                    return Some((id, EdgeConfidence::Heuristic));
                }
            }
        }

        None
    }

    /// Reconciles causal edges across microservices:
    /// 1. Resolves or filters placeholder import edges (removes dangling to == 0).
    /// 2. Links topic producers & consumers to canonical Kafka/Event topic nodes.
    /// 3. Creates direct causal dispatch edges from producers to downstream consumers.
    /// 4. Links service handlers to protobuf RPC declarations (EdgeKind::Implements).
    /// 5. Links RPC client calls to proto methods (EdgeKind::CallsRpc).
    ///
    /// Every lookup goes through the O(1) indices and edge de-duplication uses a
    /// `HashSet`, so the whole pass is linear in nodes + edges (was O(E²)).
    pub fn reconcile_edges(&mut self) {
        // 1. Resolve / filter existing edges with placeholder target (to == 0)
        let pending = std::mem::take(&mut self.edges);
        let mut resolved_edges = Vec::with_capacity(pending.len() + 64);
        for mut edge in pending {
            if edge.kind == EdgeKind::Imports && edge.to == 0 {
                let Some(target) = edge.metadata.as_deref() else {
                    continue;
                };
                // External targets (e.g. '@nestjs/common') drop the placeholder edge.
                if let Some((to_id, confidence)) = self.resolve_import_target(edge.from, target) {
                    edge.to = to_id;
                    edge.confidence = confidence;
                    resolved_edges.push(edge);
                }
            } else {
                resolved_edges.push(edge);
            }
        }
        self.edges = resolved_edges;

        let mut edge_set: HashSet<(NodeId, NodeId, EdgeKind)> =
            self.edges.iter().map(|e| (e.from, e.to, e.kind)).collect();

        // 2 & 3. Topic hubs and causal dispatch.
        let mut all_topics: Vec<CompactStr> = self
            .topic_producers
            .keys()
            .chain(self.topic_consumers.keys())
            .cloned()
            .collect();
        all_topics.sort_unstable();
        all_topics.dedup();

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

            if !producers.is_empty() && !consumers.is_empty() {
                let dispatch_meta = CompactStr::new(format!("stream:{topic_key}"));
                for &prod_id in producers {
                    for &cons_id in consumers {
                        if prod_id != cons_id
                            && edge_set.insert((prod_id, cons_id, EdgeKind::DispatchesTo))
                        {
                            self.edges.push(ContractEdge {
                                from: prod_id,
                                to: cons_id,
                                kind: EdgeKind::DispatchesTo,
                                metadata: Some(dispatch_meta.clone()),
                                // Structural: derived from producer/consumer pairing.
                                confidence: EdgeConfidence::Exact,
                            });
                        }
                    }
                }
            }
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
            // Six needles allocated once per proto method (was six per candidate node).
            let needles = [
                format!("@{bare}"),
                format!("'{bare}'"),
                format!("\"{bare}\""),
                format!("fn {bare}"),
                format!("func {bare}"),
                format!("def {bare}"),
            ];

            for h in &handlers {
                // Only an exact match against the full FQCN is trustworthy;
                // case-folding, PascalCase normalization, and substring
                // signature scraping are all bare-name heuristics that can
                // collide across unrelated same-named symbols.
                let confidence = if h.name == method_fqcn.as_str() {
                    Some(EdgeConfidence::Exact)
                } else if h.name.eq_ignore_ascii_case(bare)
                    || h.pascal_name == bare
                    || h.signature
                        .is_some_and(|s| needles.iter().any(|n| s.contains(n.as_str())))
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
        let mut proto_by_name: HashMap<String, NodeId> =
            HashMap::with_capacity(proto_methods.len() * 2);
        for (id, name) in &proto_methods {
            proto_by_name
                .entry(name.as_str().to_lowercase())
                .or_insert(*id);
            if let Some(bare) = name.split('.').next_back() {
                proto_by_name.entry(bare.to_lowercase()).or_insert(*id);
            }
        }

        let mut rpc_edges = Vec::new();
        for (caller_id, target_rpc) in &self.rpc_calls {
            let target_str = target_rpc.as_str();
            let target_bare = target_str.split('.').next_back().unwrap_or(target_str);
            // A full-name (case-insensitive) match is unambiguous; falling
            // back to the bare method name is a heuristic that can match
            // the wrong service's method of the same name.
            let matched_proto = proto_by_name
                .get(&target_str.to_lowercase())
                .map(|&id| (id, EdgeConfidence::Exact))
                .or_else(|| {
                    proto_by_name
                        .get(&target_bare.to_lowercase())
                        .map(|&id| (id, EdgeConfidence::Heuristic))
                });

            if let Some((target_id, confidence)) = matched_proto {
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

    /// O(1) in-memory reverse dependency resolution
    pub fn find_dependents(&self, target: &str) -> Vec<&ContractNode> {
        let mut result = Vec::new();
        let target_key = CompactStr::new(target);

        // 1. Direct match in reverse_deps
        if let Some(node_ids) = self.reverse_deps.get(&target_key) {
            for &id in node_ids {
                if let Some(node) = self.nodes.get(&id) {
                    result.push(node);
                }
            }
        }

        // 2. Fallback: package-level matches
        if result.is_empty() {
            for (pkg, node_ids) in &self.reverse_deps {
                if pkg.contains(target) {
                    for &id in node_ids {
                        if let Some(node) = self.nodes.get(&id) {
                            result.push(node);
                        }
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
        let mut client_stubs: Vec<(&ContractNode, EdgeConfidence)> = Vec::new();
        let mut server_handlers: Vec<(&ContractNode, EdgeConfidence)> = Vec::new();

        // Search for matching proto methods or services
        for node in self.nodes.values() {
            if !matches!(node.kind, NodeKind::GrpcService | NodeKind::GrpcMethod) {
                continue;
            }
            // Case-sensitive exact-name equality is unambiguous; case-folding
            // or a substring FQCN hit is a name heuristic.
            let exact_name = node.name.as_str() == norm_target;
            let matches_name = exact_name || node.name.eq_ignore_ascii_case(norm_target);
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

            let path_str = node.file_path.to_string_lossy();
            if Self::is_proto_file(&node.file_path) {
                proto_definition = Some(node);
            } else if path_str.contains("controller")
                || path_str.contains("handler")
                || path_str.contains("service")
            {
                server_handlers.push((node, confidence));
            } else {
                client_stubs.push((node, confidence));
            }
        }

        // If a formal proto definition is identified, resolve implementors and callers via graph edges
        if let Some(proto) = proto_definition {
            for edge in &self.edges {
                if edge.to != proto.id {
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

        // 1. Check topic registry
        for (topic_name, producer_ids) in &self.topic_producers {
            if topic_name.contains(norm_target.as_str()) {
                upstream_producers.extend(producer_ids.iter().filter_map(|id| self.nodes.get(id)));
            }
        }

        for (topic_name, consumer_ids) in &self.topic_consumers {
            if topic_name.contains(norm_target.as_str()) {
                downstream_consumers
                    .extend(consumer_ids.iter().filter_map(|id| self.nodes.get(id)));
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
        let in_scope = self
            .file_to_nodes
            .iter()
            .filter(|(path, _)| scope_filter.is_none_or(|s| path.starts_with(s)))
            .flat_map(|(_, ids)| ids.iter());
        for id in in_scope {
            if seen.contains(id) {
                continue;
            }
            if let Some(node) = self.nodes.get(id) {
                if contains_ignore_ascii_case(node.name.as_str(), query) {
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

    /// ROADMAP Item 6, short-term step: a bare-name import (ambiguous across
    /// packages) must not be reported with the same confidence as an exact
    /// fully-qualified import — and the FQCN match must land on the *right*
    /// node, not just any node sharing the bare name.
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

        let bare_edge = find_import_edge(bare_importer);
        let fqcn_edge = find_import_edge(fqcn_importer);
        let rust_edge = find_import_edge(rust_importer);

        assert_eq!(bare_edge.confidence, EdgeConfidence::Heuristic);
        assert_eq!(fqcn_edge.confidence, EdgeConfidence::Exact);
        assert_eq!(rust_edge.confidence, EdgeConfidence::Exact);

        // The whole point: the FQCN/`::` matches must resolve to the exact
        // node their qualifier names, not merely "a" node sharing the bare name.
        assert_eq!(fqcn_edge.to, acme_invoice);
        assert_eq!(rust_edge.to, acme_invoice);

        // Bug-in-the-test guard: if every match ends up Exact (or every match
        // ends up Heuristic), the confidence field is decorative, not signal.
        assert_ne!(bare_edge.confidence, fqcn_edge.confidence);
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
}
