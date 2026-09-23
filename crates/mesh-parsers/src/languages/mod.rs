pub mod cpp;
pub mod go;
pub mod java;
pub mod proto;
pub mod python;
pub mod rust_lang;
pub mod typescript;

use crate::decapitate::LanguageKind;
use crate::guard::AstGuard;
use mesh_core::{
    CompactStr, ContractGraph, ContractNode, CustomPatternConfig, NodeKind, PatternKind, RepoId,
};
use std::path::Path;

/// Everything extracted from one file, expressed against *local* node indices.
///
/// Extraction is decoupled from graph mutation so it can run on the Rayon pool
/// (`ContractGraph` is `&mut` and single-writer). `apply` assigns real `NodeId`s
/// by inserting into the graph sequentially.
#[derive(Debug, Default)]
pub struct FileIndex {
    pub nodes: Vec<ContractNode>,
    /// (local node index, imported target)
    pub dependencies: Vec<(usize, CompactStr)>,
    /// (local node index, topic)
    pub producers: Vec<(usize, CompactStr)>,
    /// (local node index, topic)
    pub consumers: Vec<(usize, CompactStr)>,
    /// (local node index, target rpc)
    pub rpc_calls: Vec<(usize, CompactStr)>,
}

impl FileIndex {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Appends `other` after `self`, shifting its local indices.
    pub fn merge(&mut self, other: FileIndex) {
        let offset = self.nodes.len();
        self.nodes.extend(other.nodes);
        self.dependencies
            .extend(other.dependencies.into_iter().map(|(i, t)| (i + offset, t)));
        self.producers
            .extend(other.producers.into_iter().map(|(i, t)| (i + offset, t)));
        self.consumers
            .extend(other.consumers.into_iter().map(|(i, t)| (i + offset, t)));
        self.rpc_calls
            .extend(other.rpc_calls.into_iter().map(|(i, t)| (i + offset, t)));
    }

    /// Inserts the extracted nodes and relations into `graph`.
    pub fn apply(self, graph: &mut ContractGraph) {
        let ids: Vec<_> = self.nodes.into_iter().map(|n| graph.add_node(n)).collect();
        for (i, target) in self.dependencies {
            graph.add_dependency(ids[i], &target);
        }
        for (i, topic) in self.producers {
            graph.add_producer(ids[i], &topic);
        }
        for (i, topic) in self.consumers {
            graph.add_consumer(ids[i], &topic);
        }
        for (i, target) in self.rpc_calls {
            graph.add_rpc_call(ids[i], &target);
        }
    }
}

/// A `CustomPatternConfig` with its regex compiled once, instead of once per file.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub kind: PatternKind,
    /// Normalised `file_pattern` (leading `*`/`.` stripped), matched by suffix or substring.
    file_needle: Option<String>,
    regex: regex::Regex,
    target_group: usize,
    consumer_group: Option<usize>,
}

impl CompiledPattern {
    pub fn compile(cfg: &CustomPatternConfig) -> Result<Self, regex::Error> {
        Ok(Self {
            kind: cfg.kind,
            file_needle: cfg.file_pattern.as_deref().map(|fp| {
                fp.trim_start_matches('*')
                    .trim_start_matches('.')
                    .to_string()
            }),
            regex: regex::Regex::new(&cfg.regex)?,
            target_group: cfg.target_group,
            consumer_group: cfg.consumer_group,
        })
    }

    /// Compiles every pattern, logging and skipping invalid ones rather than
    /// silently ignoring them on every file like the per-file compile did.
    pub fn compile_all(cfgs: &[CustomPatternConfig]) -> Vec<Self> {
        cfgs.iter()
            .filter_map(|cfg| match Self::compile(cfg) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(
                        target: "mesh::indexer",
                        "Skipping custom pattern '{}': invalid regex: {e}",
                        cfg.name
                    );
                    None
                }
            })
            .collect()
    }

    #[inline]
    fn matches_path(&self, path_str: &str) -> bool {
        match &self.file_needle {
            Some(needle) => {
                path_str.ends_with(needle.as_str()) || path_str.contains(needle.as_str())
            }
            None => true,
        }
    }
}

pub struct PolyglotIndexer;

impl PolyglotIndexer {
    /// Ingests and indexes a source file into the ContractGraph.
    pub fn index_file(file_path: &Path, content: &str, repo_id: RepoId, graph: &mut ContractGraph) {
        Self::extract(file_path, content, repo_id).apply(graph);
    }

    /// Parses a source file into a graph-independent `FileIndex`. Safe to call from any thread.
    pub fn extract(file_path: &Path, content: &str, repo_id: RepoId) -> FileIndex {
        let path_str = file_path.to_string_lossy();
        let lang_kind = LanguageKind::from_path(&path_str);
        let mut out = FileIndex::default();

        match lang_kind {
            LanguageKind::Protobuf => {
                out.nodes = proto::ProtoExtractor::extract(file_path, content, repo_id);
            }
            LanguageKind::Java => {
                if let Some(nodes) = AstGuard::with_parser(lang_kind, |parser| {
                    java::JavaExtractor::extract(file_path, content, repo_id, parser)
                }) {
                    for (i, node) in nodes.iter().enumerate() {
                        if node.kind == NodeKind::KafkaTopic {
                            out.consumers.push((i, node.name.clone()));
                        }
                    }
                    out.nodes = nodes;
                }
            }
            LanguageKind::Go => {
                if let Some(nodes) = AstGuard::with_parser(lang_kind, |parser| {
                    go::GoExtractor::extract(file_path, content, repo_id, parser)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Python => {
                if let Some((nodes, relations)) = AstGuard::with_parser(lang_kind, |parser| {
                    python::PythonExtractor::extract_with_relations(
                        file_path, content, repo_id, parser,
                    )
                }) {
                    out.nodes = nodes;
                    out.dependencies = relations.dependencies;
                    out.producers = relations.producers;
                    out.consumers = relations.consumers;
                }
            }
            LanguageKind::TypeScript => {
                let mut imports = Vec::new();
                let nodes = AstGuard::with_parser(lang_kind, |parser| {
                    typescript::TypeScriptExtractor::extract(
                        file_path,
                        content,
                        repo_id,
                        parser,
                        &mut imports,
                    )
                })
                .unwrap_or_default();

                if !imports.is_empty() {
                    let content_lines: Vec<&str> = content.lines().collect();
                    for (i, node) in nodes.iter().enumerate() {
                        let body = content_lines
                            .get(node.line_start.saturating_sub(1)..node.line_end)
                            .unwrap_or(&[]);

                        for (_sym, imported) in &imports {
                            if imported.is_empty() {
                                continue;
                            }
                            let is_module_path_entry =
                                imported.contains('/') || imported.starts_with('.');
                            let is_used = is_module_path_entry
                                || body.iter().any(|l| l.contains(imported.as_str()));
                            if is_used {
                                out.dependencies.push((i, CompactStr::new(imported)));
                            }
                        }
                    }
                }
                out.nodes = nodes;
            }
            LanguageKind::Rust => {
                if let Some(nodes) = AstGuard::with_parser(lang_kind, |parser| {
                    rust_lang::RustExtractor::extract(file_path, content, repo_id, parser)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Cpp => {
                if let Some(nodes) = AstGuard::with_parser(lang_kind, |parser| {
                    cpp::CppExtractor::extract(file_path, content, repo_id, parser)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Yaml => {
                Self::extract_yaml_contracts(file_path, content, repo_id, &mut out);
            }
            LanguageKind::Unknown => {}
        }

        out
    }

    /// Evaluates user-defined declarative patterns from TOML configuration.
    /// Compiles the regexes on every call — prefer `extract_custom_patterns` with
    /// `CompiledPattern::compile_all` done once per run.
    pub fn apply_custom_patterns(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        patterns: &[CustomPatternConfig],
        graph: &mut ContractGraph,
    ) {
        if patterns.is_empty() {
            return;
        }
        let compiled = CompiledPattern::compile_all(patterns);
        Self::extract_custom_patterns(file_path, content, repo_id, &compiled).apply(graph);
    }

    /// Runs precompiled declarative patterns over `content`. Safe to call from any thread.
    pub fn extract_custom_patterns(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        patterns: &[CompiledPattern],
    ) -> FileIndex {
        let mut out = FileIndex::default();
        if patterns.is_empty() {
            return out;
        }

        let path_str = file_path.to_string_lossy();
        let mut package_name: Option<CompactStr> = None;
        let interned: mesh_core::FilePath = std::sync::Arc::from(file_path);

        for pat in patterns {
            if !pat.matches_path(&path_str) {
                continue;
            }

            for caps in pat.regex.captures_iter(content) {
                let target = caps
                    .get(pat.target_group)
                    .map(|m| m.as_str().trim())
                    .unwrap_or("");
                if target.is_empty() {
                    continue;
                }

                let consumer_name = pat
                    .consumer_group
                    .and_then(|idx| caps.get(idx))
                    .map(|m| m.as_str().trim())
                    .unwrap_or(target);

                let (node_name, kind, signature) = match pat.kind {
                    PatternKind::TopicProducer => (
                        format!("produce:{target}"),
                        NodeKind::EventStream,
                        format!("Producer of {target}"),
                    ),
                    PatternKind::TopicConsumer => (
                        if consumer_name != target {
                            consumer_name.to_string()
                        } else {
                            format!("consume:{target}")
                        },
                        NodeKind::PostProcessor,
                        format!("Consumer of {target}"),
                    ),
                    PatternKind::Saga => (
                        target.to_string(),
                        NodeKind::Saga,
                        format!("Saga: {target}"),
                    ),
                    PatternKind::Rpc => (
                        format!("rpc:{target}"),
                        NodeKind::GrpcMethod,
                        format!("RPC call: {target}"),
                    ),
                };

                let package = package_name
                    .get_or_insert_with(|| mesh_core::detect_service_package(file_path, None))
                    .clone();

                let idx = out.nodes.len();
                out.nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(node_name),
                    kind,
                    file_path: interned.clone(),
                    line_start: 1,
                    line_end: 1,
                    package,
                    repo_id,
                    signature: Some(CompactStr::new(signature)),
                    docstring: None,
                });

                match pat.kind {
                    PatternKind::TopicProducer => {
                        out.producers.push((idx, CompactStr::new(target)))
                    }
                    PatternKind::TopicConsumer => {
                        out.consumers.push((idx, CompactStr::new(target)))
                    }
                    PatternKind::Rpc => out.rpc_calls.push((idx, CompactStr::new(target))),
                    PatternKind::Saga => {}
                }
            }
        }

        out
    }

    fn extract_yaml_contracts(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        out: &mut FileIndex,
    ) {
        let path_str = file_path.to_string_lossy().to_lowercase();
        let interned: mesh_core::FilePath = std::sync::Arc::from(file_path);

        // Check for AsyncAPI spec
        if path_str.contains("asyncapi") || content.contains("asyncapi:") {
            if let Ok(yaml_val) = serde_yaml::from_str::<serde_yaml::Value>(content) {
                if let Some(channels) = yaml_val.get("channels").and_then(|c| c.as_mapping()) {
                    for (ch_name, _) in channels {
                        if let Some(name_str) = ch_name.as_str() {
                            let node = ContractNode {
                                id: 0,
                                name: CompactStr::new(name_str),
                                kind: NodeKind::EventStream,
                                file_path: interned.clone(),
                                line_start: 1,
                                line_end: 1,
                                package: CompactStr::new("asyncapi"),
                                repo_id,
                                signature: Some(CompactStr::new(format!("channel {name_str}"))),
                                docstring: None,
                            };
                            out.producers
                                .push((out.nodes.len(), CompactStr::new(name_str)));
                            out.nodes.push(node);
                        }
                    }
                }
            }
        }

        // Check for OpenAPI spec
        if path_str.contains("openapi")
            || content.contains("openapi:")
            || content.contains("swagger:")
        {
            if let Ok(yaml_val) = serde_yaml::from_str::<serde_yaml::Value>(content) {
                if let Some(paths) = yaml_val.get("paths").and_then(|p| p.as_mapping()) {
                    for (path_name, methods) in paths {
                        if let Some(p_str) = path_name.as_str() {
                            if let Some(m_map) = methods.as_mapping() {
                                for (method_name, _) in m_map {
                                    if let Some(m_str) = method_name.as_str() {
                                        let ep_name = format!("{} {}", m_str.to_uppercase(), p_str);
                                        let node = ContractNode {
                                            id: 0,
                                            name: CompactStr::new(&ep_name),
                                            kind: NodeKind::HttpEndpoint,
                                            file_path: interned.clone(),
                                            line_start: 1,
                                            line_end: 1,
                                            package: CompactStr::new("openapi"),
                                            repo_id,
                                            signature: Some(CompactStr::new(&ep_name)),
                                            docstring: None,
                                        };
                                        out.nodes.push(node);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_polyglot_indexer_cpp() {
        let mut graph = ContractGraph::new();
        let cpp = "namespace billing {\nclass Invoice {};\nvoid HandleCharge(int amount) {}\n}\n";
        PolyglotIndexer::index_file(
            Path::new("services/billing/invoice.hpp"),
            cpp,
            0,
            &mut graph,
        );
        assert_eq!(graph.node_count(), 2);
        assert!(graph.all_nodes().any(|n| n.name == "Invoice"));
        assert!(graph
            .all_nodes()
            .any(|n| n.name == "HandleCharge" && n.kind == NodeKind::HttpEndpoint));
    }

    #[test]
    fn test_polyglot_indexer_proto() {
        let mut graph = ContractGraph::new();
        let proto = "syntax = \"proto3\"; package test.v1; service TestService { rpc DoTest (Req) returns (Resp); }";
        PolyglotIndexer::index_file(Path::new("test.proto"), proto, 0, &mut graph);
        assert_eq!(graph.node_count(), 2);
    }

    #[test]
    fn test_polyglot_indexer_asyncapi() {
        let mut graph = ContractGraph::new();
        let yaml = r#"
asyncapi: 2.6.0
channels:
  billing.events:
    description: Billing event topic
"#;
        PolyglotIndexer::index_file(Path::new("asyncapi.yaml"), yaml, 0, &mut graph);
        assert_eq!(graph.node_count(), 1);
        let impact = graph.analyze_impact("billing.events");
        assert!(!impact.topics.is_empty());
    }

    #[test]
    fn test_declarative_custom_patterns() {
        let mut graph = ContractGraph::new();
        let patterns = vec![
            CustomPatternConfig {
                name: "outbox_producer".to_string(),
                kind: PatternKind::TopicProducer,
                file_pattern: Some("*.ts".to_string()),
                regex: r#"createEvent<([^>]+)>"#.to_string(),
                target_group: 1,
                consumer_group: None,
            },
            CustomPatternConfig {
                name: "post_processor_consumer".to_string(),
                kind: PatternKind::TopicConsumer,
                file_pattern: Some("*.ts".to_string()),
                regex: r#"class\s+(\w+)\s+extends\s+\w*PostProcessor<([^>]+)>"#.to_string(),
                target_group: 2,
                consumer_group: Some(1),
            },
        ];

        let ts_producer = "await EventQueueEntity.createEvent<UserCreatedEvent>(event);";
        let ts_consumer = "export class UserCreatedPostProcessor extends SinglePostProcessor<UserCreatedEvent> {}";

        PolyglotIndexer::apply_custom_patterns(
            Path::new("services/user/producer.ts"),
            ts_producer,
            1,
            &patterns,
            &mut graph,
        );
        PolyglotIndexer::apply_custom_patterns(
            Path::new("services/user/consumer.ts"),
            ts_consumer,
            1,
            &patterns,
            &mut graph,
        );

        let impact = graph.analyze_impact("UserCreatedEvent");
        assert_eq!(impact.upstream_producers.len(), 1);
        assert_eq!(impact.downstream_consumers.len(), 1);
        assert_eq!(
            impact.downstream_consumers[0].name.as_str(),
            "UserCreatedPostProcessor"
        );
    }
}
