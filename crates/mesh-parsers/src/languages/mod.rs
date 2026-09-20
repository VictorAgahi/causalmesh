pub mod go;
pub mod java;
pub mod proto;
pub mod python;
pub mod rust_lang;
pub mod typescript;

use crate::decapitate::LanguageKind;
use crate::guard::AstGuard;
use mesh_core::{CompactStr, ContractGraph, ContractNode, NodeKind, RepoId};
use std::path::Path;

pub struct PolyglotIndexer;

impl PolyglotIndexer {
    /// Ingests and indexes a source file into the ContractGraph
    pub fn index_file(file_path: &Path, content: &str, repo_id: RepoId, graph: &mut ContractGraph) {
        let path_str = file_path.to_string_lossy();
        let lang_kind = LanguageKind::from_path(&path_str);

        match lang_kind {
            LanguageKind::Protobuf => {
                let nodes = proto::ProtoExtractor::extract(file_path, content, repo_id);
                for node in nodes {
                    graph.add_node(node);
                }
            }
            LanguageKind::Java => {
                let lang = tree_sitter_java::LANGUAGE.into();
                if let Ok(mut parser) = AstGuard::create_bounded_parser(&lang) {
                    let nodes =
                        java::JavaExtractor::extract(file_path, content, repo_id, &mut parser);
                    for node in nodes {
                        let is_kafka = node.kind == NodeKind::KafkaTopic;
                        let topic_name = node.name.clone();
                        let node_id = graph.add_node(node);
                        if is_kafka {
                            graph.add_consumer(node_id, topic_name.as_str());
                        }
                    }
                }
            }
            LanguageKind::Go => {
                let lang = tree_sitter_go::LANGUAGE.into();
                if let Ok(mut parser) = AstGuard::create_bounded_parser(&lang) {
                    let nodes = go::GoExtractor::extract(file_path, content, repo_id, &mut parser);
                    for node in nodes {
                        graph.add_node(node);
                    }
                }
            }
            LanguageKind::Python => {
                let lang = tree_sitter_python::LANGUAGE.into();
                if let Ok(mut parser) = AstGuard::create_bounded_parser(&lang) {
                    let nodes =
                        python::PythonExtractor::extract(file_path, content, repo_id, &mut parser);
                    for node in nodes {
                        graph.add_node(node);
                    }
                }
            }
            LanguageKind::TypeScript => {
                let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
                if let Ok(mut parser) = AstGuard::create_bounded_parser(&lang) {
                    let mut imports = Vec::new();
                    let nodes = typescript::TypeScriptExtractor::extract(
                        file_path,
                        content,
                        repo_id,
                        &mut parser,
                        &mut imports,
                    );
                    for node in nodes {
                        let node_id = graph.add_node(node);
                        for (_sym, imported) in &imports {
                            graph.add_dependency(node_id, imported);
                        }
                    }
                }
            }
            LanguageKind::Rust => {
                let lang = tree_sitter_rust::LANGUAGE.into();
                if let Ok(mut parser) = AstGuard::create_bounded_parser(&lang) {
                    let nodes =
                        rust_lang::RustExtractor::extract(file_path, content, repo_id, &mut parser);
                    for node in nodes {
                        graph.add_node(node);
                    }
                }
            }
            LanguageKind::Yaml => {
                Self::extract_yaml_contracts(file_path, content, repo_id, graph);
            }
            LanguageKind::Unknown => {}
        }
    }

    fn extract_yaml_contracts(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        graph: &mut ContractGraph,
    ) {
        let path_str = file_path.to_string_lossy().to_lowercase();

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
                                file_path: file_path.to_path_buf(),
                                line_start: 1,
                                line_end: 1,
                                package: CompactStr::new("asyncapi"),
                                repo_id,
                                signature: Some(CompactStr::new(format!("channel {name_str}"))),
                                docstring: None,
                            };
                            let nid = graph.add_node(node);
                            graph.add_producer(nid, name_str);
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
                                            file_path: file_path.to_path_buf(),
                                            line_start: 1,
                                            line_end: 1,
                                            package: CompactStr::new("openapi"),
                                            repo_id,
                                            signature: Some(CompactStr::new(&ep_name)),
                                            docstring: None,
                                        };
                                        graph.add_node(node);
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
}
