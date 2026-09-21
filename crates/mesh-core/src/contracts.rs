use crate::types::CompactStr;
use crate::types::{ContractEdge, ContractNode, EdgeKind, NodeId, NodeKind, RepoId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcTrace {
    pub target: CompactStr,
    pub proto_definition: Option<ContractNode>,
    pub client_stubs: Vec<ContractNode>,
    pub server_handlers: Vec<ContractNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactFlow {
    pub target: CompactStr,
    pub upstream_producers: Vec<ContractNode>,
    pub topics: Vec<ContractNode>,
    pub downstream_consumers: Vec<ContractNode>,
    pub related_sagas: Vec<ContractNode>,
}

/// In-memory graph of polyglot architecture contracts and dependencies.
#[derive(Debug, Clone, Default)]
pub struct ContractGraph {
    nodes: HashMap<NodeId, ContractNode>,
    edges: Vec<ContractEdge>,

    // O(1) in-memory indices
    name_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    package_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
    file_to_nodes: HashMap<PathBuf, Vec<NodeId>>,
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
        let stale_ids: Vec<NodeId> = self.file_to_nodes.remove(file_path).unwrap_or_default();

        if stale_ids.is_empty() {
            return;
        }

        let stale_set: std::collections::HashSet<NodeId> = stale_ids.iter().copied().collect();

        // Remove nodes from primary store
        for &id in &stale_ids {
            self.nodes.remove(&id);
        }

        // Purge from secondary indices
        self.name_to_nodes.retain(|_, ids| {
            ids.retain(|id| !stale_set.contains(id));
            !ids.is_empty()
        });
        self.package_to_nodes.retain(|_, ids| {
            ids.retain(|id| !stale_set.contains(id));
            !ids.is_empty()
        });
        self.fqcn_to_node.retain(|_, id| !stale_set.contains(id));
        self.reverse_deps.retain(|_, ids| {
            ids.retain(|id| !stale_set.contains(id));
            !ids.is_empty()
        });
        self.topic_producers.retain(|_, ids| {
            ids.retain(|id| !stale_set.contains(id));
            !ids.is_empty()
        });
        self.topic_consumers.retain(|_, ids| {
            ids.retain(|id| !stale_set.contains(id));
            !ids.is_empty()
        });
        self.rpc_calls.retain(|(id, _)| !stale_set.contains(id));

        // Remove edges referencing stale nodes
        self.edges
            .retain(|e| !stale_set.contains(&e.from) && !stale_set.contains(&e.to));
    }

    /// Reconciles causal edges across microservices:
    /// 1. Links topic producers & consumers to canonical Kafka/Event topic nodes.
    /// 2. Creates direct causal dispatch edges from producers to downstream consumers.
    /// 3. Links service handlers to protobuf RPC declarations (EdgeKind::Implements).
    /// 4. Links RPC client calls to proto methods (EdgeKind::CallsRpc).
    /// 5. Resolves or filters placeholder import edges (removes dangling to == 0).
    pub fn reconcile_edges(&mut self) {
        // 1. Resolve / filter existing edges with placeholder target (to == 0)
        let mut resolved_edges = Vec::with_capacity(self.edges.len() + 64);
        for mut edge in self.edges.drain(..) {
            if edge.kind == EdgeKind::Imports && edge.to == 0 {
                if let Some(ref target) = edge.metadata {
                    let target_str = target.as_str();
                    // Path::file_stem() strips everything after the LAST '.', so it
                    // treats a scoped package ("@volontariapp/contracts" -> "contracts")
                    // exactly the same as a dotted relative filename
                    // ("./post.endpoints" -> "post", discarding ".endpoints" as if it
                    // were an extension). Both are intentional here — see below — but
                    // this is why the two cases must never share a matching branch.
                    let target_stem = Path::new(target_str)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(target_str);

                    let is_relative_or_absolute_path =
                        target_str.starts_with('.') || target_str.starts_with('/');
                    // NOTE: matching a scoped package specifier ("@volontariapp/contracts")
                    // against `n.package` was tried and reverted — the extractor only ever
                    // records the raw "from '...'" module string, never which SPECIFIC
                    // named symbol was imported (`import { A, B, C } from 'x'` collapses to
                    // just "x"). Any package-name match therefore has to `.find()` an
                    // arbitrary node that merely shares that package, in HashMap iteration
                    // order — observed live picking an unrelated symbol out of dozens of
                    // real candidates. A silently wrong specific edge is worse than no
                    // edge; fixing this for real needs the extractor to capture and resolve
                    // each named import individually, not a matching-branch tweak here.
                    let is_qualified = target_str.contains('/') || target_str.contains('.');

                    // Relative imports can only ever resolve to a file inside the SAME
                    // repo as the importer — never across a repo boundary. Without this,
                    // a generic stem like "post" (from "../endpoints/post.endpoints")
                    // matches any same-named symbol anywhere in the whole multi-repo
                    // graph (e.g. an unrelated `post()` HTTP helper method in a
                    // completely different repo's e2e test helpers).
                    let importer_repo_id = self.nodes.get(&edge.from).map(|n| n.repo_id);

                    let matched_id = self
                        .nodes
                        .values()
                        .find(|n| {
                            n.name.as_str() == target_str
                                || (is_relative_or_absolute_path
                                    && n.name.as_str() == target_stem
                                    && importer_repo_id == Some(n.repo_id))
                                || (is_qualified
                                    && !is_relative_or_absolute_path
                                    && n.package.as_str() == target_str)
                                || format!("{}.{}", n.package, n.name) == target_str
                        })
                        .map(|n| n.id);

                    if let Some(to_id) = matched_id {
                        edge.to = to_id;
                        resolved_edges.push(edge);
                    }
                    // If target is external (e.g. '@nestjs/common'), drop placeholder edge to 0.
                }
            } else {
                resolved_edges.push(edge);
            }
        }
        self.edges = resolved_edges;

        let mut all_topics = std::collections::BTreeSet::new();
        for t in self.topic_producers.keys() {
            all_topics.insert(t.clone());
        }
        for t in self.topic_consumers.keys() {
            all_topics.insert(t.clone());
        }

        for topic_key in all_topics {
            // Find existing topic node or create a canonical one
            let existing_topic_id = self
                .nodes
                .values()
                .find(|n| {
                    n.package == "event-bus"
                        && n.kind == NodeKind::EventStream
                        && n.name.eq_ignore_ascii_case(topic_key.as_str())
                })
                .map(|n| n.id);

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
                        file_path: PathBuf::from("event-bus"),
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

            // Edge: Producer -> Topic (Produces)
            if let Some(producers) = self.topic_producers.get(&topic_key).cloned() {
                for prod_id in producers {
                    let exists = self.edges.iter().any(|e| {
                        e.from == prod_id && e.to == topic_id && e.kind == EdgeKind::Produces
                    });
                    if !exists {
                        self.edges.push(ContractEdge {
                            from: prod_id,
                            to: topic_id,
                            kind: EdgeKind::Produces,
                            metadata: Some(topic_key.clone()),
                        });
                    }
                }
            }

            // Edge: Topic -> Consumer (Consumes)
            if let Some(consumers) = self.topic_consumers.get(&topic_key).cloned() {
                for cons_id in consumers {
                    let exists = self.edges.iter().any(|e| {
                        e.from == topic_id && e.to == cons_id && e.kind == EdgeKind::Consumes
                    });
                    if !exists {
                        self.edges.push(ContractEdge {
                            from: topic_id,
                            to: cons_id,
                            kind: EdgeKind::Consumes,
                            metadata: Some(topic_key.clone()),
                        });
                    }
                }
            }

            // Edge: Direct Causal Dispatch (Producer -> Consumer)
            if let (Some(prods), Some(cons)) = (
                self.topic_producers.get(&topic_key).cloned(),
                self.topic_consumers.get(&topic_key).cloned(),
            ) {
                for prod_id in &prods {
                    for cons_id in &cons {
                        if prod_id != cons_id {
                            let exists = self.edges.iter().any(|e| {
                                e.from == *prod_id
                                    && e.to == *cons_id
                                    && e.kind == EdgeKind::DispatchesTo
                            });
                            if !exists {
                                self.edges.push(ContractEdge {
                                    from: *prod_id,
                                    to: *cons_id,
                                    kind: EdgeKind::DispatchesTo,
                                    metadata: Some(CompactStr::new(format!("stream:{topic_key}"))),
                                });
                            }
                        }
                    }
                }
            }
        }

        // 3. Reconcile gRPC Proto Definitions <-> Service Handlers (Implements)
        let proto_methods: Vec<(NodeId, CompactStr)> = self
            .nodes
            .values()
            .filter(|n| {
                n.file_path.to_string_lossy().ends_with(".proto") && n.kind == NodeKind::GrpcMethod
            })
            .map(|n| (n.id, n.name.clone()))
            .collect();

        for (proto_id, method_fqcn) in &proto_methods {
            let bare_method_name = method_fqcn
                .split('.')
                .next_back()
                .unwrap_or(method_fqcn.as_str());

            let implementor_ids: Vec<NodeId> = self
                .nodes
                .values()
                .filter(|n| {
                    if n.file_path.to_string_lossy().ends_with(".proto") {
                        return false;
                    }
                    // Only nodes the language extractors actually tagged as a gRPC
                    // handler (e.g. TS `@GrpcMethod`/`@GrpcService`) may implement an
                    // RPC. Without this, any same-named REST handler, factory method,
                    // or unrelated function matches by bare name alone.
                    if n.kind != NodeKind::GrpcMethod && n.kind != NodeKind::GrpcService {
                        return false;
                    }
                    if n.name.starts_with("rpc:")
                        || n.name.starts_with("produce:")
                        || n.name.starts_with("consume:")
                    {
                        return false;
                    }

                    n.name == *method_fqcn
                        || n.name.as_str().eq_ignore_ascii_case(bare_method_name)
                        || crate::types::to_pascal_case(n.name.as_str()) == bare_method_name
                        || n.signature
                            .as_ref()
                            .map(|s| {
                                s.contains(&format!("@{bare_method_name}"))
                                    || s.contains(&format!("'{bare_method_name}'"))
                                    || s.contains(&format!("\"{bare_method_name}\""))
                                    || s.contains(&format!("fn {bare_method_name}"))
                                    || s.contains(&format!("func {bare_method_name}"))
                                    || s.contains(&format!("def {bare_method_name}"))
                            })
                            .unwrap_or(false)
                })
                .map(|n| n.id)
                .collect();

            for impl_id in implementor_ids {
                let exists = self.edges.iter().any(|e| {
                    e.from == impl_id && e.to == *proto_id && e.kind == EdgeKind::Implements
                });
                if !exists {
                    self.edges.push(ContractEdge {
                        from: impl_id,
                        to: *proto_id,
                        kind: EdgeKind::Implements,
                        metadata: Some(method_fqcn.clone()),
                    });
                }
            }
        }

        // 4. Reconcile RPC Client Calls (CallsRpc)
        let rpc_calls = self.rpc_calls.clone();
        for (caller_id, target_rpc) in rpc_calls {
            let target_str = target_rpc.as_str();
            let target_bare = target_str.split('.').next_back().unwrap_or(target_str);

            let matched_proto = proto_methods
                .iter()
                .find(|(_, m_name)| {
                    m_name.as_str().eq_ignore_ascii_case(target_str)
                        || m_name
                            .split('.')
                            .next_back()
                            .unwrap_or("")
                            .eq_ignore_ascii_case(target_bare)
                })
                .map(|(id, _)| *id);

            if let Some(target_id) = matched_proto {
                let exists = self.edges.iter().any(|e| {
                    e.from == caller_id && e.to == target_id && e.kind == EdgeKind::CallsRpc
                });
                if !exists {
                    self.edges.push(ContractEdge {
                        from: caller_id,
                        to: target_id,
                        kind: EdgeKind::CallsRpc,
                        metadata: Some(target_rpc.clone()),
                    });
                }
            }
        }
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
    pub fn analyze_grpc(&self, target: &str) -> GrpcTrace {
        let norm_target = target.trim();
        let mut proto_definition = None;
        let mut client_stubs = Vec::new();
        let mut server_handlers = Vec::new();

        // Search for matching proto methods or services
        for node in self.nodes.values() {
            let matches_name = node.name.eq_ignore_ascii_case(norm_target);
            let matches_fqcn = node.package.contains(norm_target)
                || format!("{}/{}", node.package, node.name).contains(norm_target);

            if matches_name || matches_fqcn {
                match node.kind {
                    NodeKind::GrpcService | NodeKind::GrpcMethod => {
                        let path_str = node.file_path.to_string_lossy();
                        if path_str.ends_with(".proto") {
                            proto_definition = Some(node.clone());
                        } else if path_str.contains("controller")
                            || path_str.contains("handler")
                            || path_str.contains("service")
                        {
                            server_handlers.push(node.clone());
                        } else {
                            client_stubs.push(node.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        // If a formal proto definition is identified, resolve implementors and callers via graph edges
        if let Some(ref proto) = proto_definition {
            for edge in &self.edges {
                if edge.to == proto.id && edge.kind == EdgeKind::Implements {
                    if let Some(handler) = self.nodes.get(&edge.from) {
                        if !server_handlers.iter().any(|h| h.id == handler.id) {
                            server_handlers.push(handler.clone());
                        }
                    }
                } else if edge.to == proto.id && edge.kind == EdgeKind::CallsRpc {
                    if let Some(client) = self.nodes.get(&edge.from) {
                        if !client_stubs.iter().any(|c| c.id == client.id) {
                            client_stubs.push(client.clone());
                        }
                    }
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

    /// Asynchronous causal impact flow resolution
    pub fn analyze_impact(&self, target: &str) -> ImpactFlow {
        let norm_target = target.trim().to_lowercase();
        let mut upstream_producers = Vec::new();
        let mut topics = Vec::new();
        let mut downstream_consumers = Vec::new();
        let mut related_sagas = Vec::new();

        // 1. Check topic registry
        for (topic_name, producer_ids) in &self.topic_producers {
            if topic_name.contains(&norm_target) {
                for &pid in producer_ids {
                    if let Some(p_node) = self.nodes.get(&pid) {
                        upstream_producers.push(p_node.clone());
                    }
                }
            }
        }

        for (topic_name, consumer_ids) in &self.topic_consumers {
            if topic_name.contains(&norm_target) {
                for &cid in consumer_ids {
                    if let Some(c_node) = self.nodes.get(&cid) {
                        downstream_consumers.push(c_node.clone());
                    }
                }
            }
        }

        // 2. Check nodes for topics and sagas
        for node in self.nodes.values() {
            let lower_name = node.name.to_lowercase();
            if lower_name.contains(&norm_target) {
                match node.kind {
                    NodeKind::KafkaTopic | NodeKind::EventStream | NodeKind::Queue => {
                        topics.push(node.clone());
                    }
                    NodeKind::Saga => {
                        related_sagas.push(node.clone());
                    }
                    NodeKind::PostProcessor => {
                        downstream_consumers.push(node.clone());
                    }
                    _ => {}
                }
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

    /// Search symbol declarations in memory
    pub fn search_symbols(&self, query: &str, scope_filter: Option<&Path>) -> Vec<&ContractNode> {
        let lower_query = query.to_lowercase();
        let mut matches = Vec::new();

        for node in self.nodes.values() {
            if let Some(scope) = scope_filter {
                if !node.file_path.starts_with(scope) {
                    continue;
                }
            }

            if node.name.to_lowercase().contains(&lower_query) {
                matches.push(node);
            }
        }

        matches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_dependency_graph() {
        let mut graph = ContractGraph::new();
        let consumer_node = ContractNode {
            id: 0,
            name: CompactStr::new("AuthConsumer"),
            kind: NodeKind::ServiceClass,
            file_path: PathBuf::from("services/auth/AuthConsumer.java"),
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
            file_path: PathBuf::from("proto-registry/auth.proto"),
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

    #[test]
    fn test_large_scale_repo_indexing() {
        let mut graph = ContractGraph::new();

        // Register 500 microservices (exceeding 255 u8 limit)
        for repo_idx in 0..500u16 {
            let node = ContractNode {
                id: 0,
                name: CompactStr::new(format!("ServiceHandler{repo_idx}")),
                kind: NodeKind::ServiceClass,
                file_path: PathBuf::from(format!("services/service_{repo_idx}/Handler.go")),
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
