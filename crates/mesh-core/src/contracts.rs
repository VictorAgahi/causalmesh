use crate::types::CompactStr;
use crate::types::{ContractEdge, ContractNode, EdgeKind, NodeId, NodeKind};
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
