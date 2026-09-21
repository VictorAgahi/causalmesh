use mesh_core::{CompactStr, ContractNode, NodeKind, RepoId};
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct JavaExtractor;

impl JavaExtractor {
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        let mut nodes = Vec::new();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return nodes,
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = CompactStr::default();

        Self::visit_node(
            root,
            source_bytes,
            file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
        );
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &Path,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        match node.kind() {
            "package_declaration" => {
                if let Ok(text) = node.utf8_text(source) {
                    let clean = text
                        .trim_start_matches("package ")
                        .trim_end_matches(';')
                        .trim();
                    *package_name = mesh_core::detect_service_package(file_path, Some(clean));
                }
            }
            "class_declaration" | "interface_declaration" => {
                let class_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownClass");

                let mut kind = if node.kind() == "interface_declaration" {
                    NodeKind::Interface
                } else {
                    NodeKind::ServiceClass
                };

                let node_text = node.utf8_text(source).unwrap_or("");
                if node_text.contains("@GrpcService") {
                    kind = NodeKind::GrpcService;
                }

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(class_name),
                    kind,
                    file_path: file_path.to_path_buf(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(format!("class {class_name}"))),
                    docstring: None,
                });
            }
            "method_declaration" => {
                let method_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownMethod");

                let mut kind = NodeKind::ServiceClass;
                let mut topic_target = None;

                // Extract annotations from method text or modifiers
                let mut full_anno = String::new();
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "modifiers" || child.kind().contains("annotation") {
                        if let Ok(t) = child.utf8_text(source) {
                            full_anno.push_str(t);
                            full_anno.push(' ');
                        }
                    }
                }
                if full_anno.is_empty() {
                    if let Ok(t) = node.utf8_text(source) {
                        full_anno = t.to_string();
                    }
                }

                if full_anno.contains("@KafkaListener") {
                    kind = NodeKind::KafkaTopic;
                    topic_target = Some(Self::extract_annotation_param(&full_anno, "topics"));
                } else if full_anno.contains("@GetMapping")
                    || full_anno.contains("@PostMapping")
                    || full_anno.contains("@RequestMapping")
                {
                    kind = NodeKind::HttpEndpoint;
                }

                let sig_str = if let Ok(text) = node.utf8_text(source) {
                    let first_line = text.lines().next().unwrap_or("").trim();
                    Some(CompactStr::new(first_line))
                } else {
                    None
                };

                let final_name = if let Some(t) = topic_target {
                    CompactStr::new(t)
                } else {
                    CompactStr::new(method_name)
                };

                nodes.push(ContractNode {
                    id: 0,
                    name: final_name,
                    kind,
                    file_path: file_path.to_path_buf(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: sig_str,
                    docstring: None,
                });
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, file_path, repo_id, package_name, nodes);
        }
    }

    fn extract_annotation_param(annotation: &str, param: &str) -> String {
        if let Some(idx) = annotation.find(param) {
            let rest = &annotation[idx + param.len()..];
            if let Some(quote_start) = rest.find('"') {
                if let Some(quote_end) = rest[quote_start + 1..].find('"') {
                    return rest[quote_start + 1..quote_start + 1 + quote_end].to_string();
                }
            }
        }
        "unknown.topic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_java_extractor() {
        let code = r#"
package com.mesh.billing;

@RestController
public class BillingController {
    @GetMapping("/bills")
    public List<Bill> getBills() {
        return null;
    }

    @KafkaListener(topics = "billing.events")
    public void onEvent(String msg) {}
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let nodes =
            JavaExtractor::extract(Path::new("BillingController.java"), code, 1, &mut parser);
        assert!(nodes.iter().any(|n| n.name == "BillingController"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "getBills" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "billing.events" && n.kind == NodeKind::KafkaTopic));
    }
}
