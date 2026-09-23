use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

/// `class_declaration`, `object_declaration` and `function_declaration` all expose a `name`
/// field in `tree-sitter-kotlin-ng`'s grammar, same as Java. Interface-vs-class and
/// annotations have no dedicated field, though: Kotlin represents `interface` as an
/// anonymous keyword child of `class_declaration`, and annotations live inside a sibling
/// `modifiers` node, so those are found by scanning direct children by kind.
pub struct KotlinExtractor;

impl KotlinExtractor {
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return nodes,
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = mesh_core::detect_service_package(&file_path, None);

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
        );
        nodes
    }

    fn direct_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        let found = node.children(&mut cursor).find(|c| c.kind() == kind);
        found
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        match node.kind() {
            "package_header" => {
                let mut cursor = node.walk();
                let dotted_name = node
                    .children(&mut cursor)
                    .find(|c| c.kind() == "qualified_identifier" || c.kind() == "identifier")
                    .and_then(|n| n.utf8_text(source).ok());
                if let Some(text) = dotted_name {
                    *package_name = mesh_core::detect_service_package(file_path, Some(text));
                }
            }
            "class_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let is_interface = Self::direct_child_of_kind(node, "interface").is_some();
                    let modifiers_text = Self::direct_child_of_kind(node, "modifiers")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");

                    let kind = if is_interface {
                        NodeKind::Interface
                    } else if modifiers_text.contains("@GrpcService") {
                        NodeKind::GrpcService
                    } else {
                        NodeKind::ServiceClass
                    };

                    let first_line = Self::first_line(node, source, name);

                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(name),
                        kind,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                }
            }
            "object_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let first_line = Self::first_line(node, source, name);
                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(name),
                        kind: NodeKind::ServiceClass,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                }
            }
            "function_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let modifiers_text = Self::direct_child_of_kind(node, "modifiers")
                        .and_then(|n| n.utf8_text(source).ok())
                        .unwrap_or("");

                    let mut kind = NodeKind::ServiceClass;
                    let mut topic_target = None;

                    if modifiers_text.contains("@KafkaListener") {
                        kind = NodeKind::KafkaTopic;
                        topic_target =
                            Some(Self::extract_annotation_param(modifiers_text, "topics"));
                    } else if modifiers_text.contains("@GetMapping")
                        || modifiers_text.contains("@PostMapping")
                        || modifiers_text.contains("@PutMapping")
                        || modifiers_text.contains("@DeleteMapping")
                        || modifiers_text.contains("@RequestMapping")
                    {
                        kind = NodeKind::HttpEndpoint;
                    }

                    let first_line = Self::first_line(node, source, name);
                    let final_name = match topic_target {
                        Some(t) => CompactStr::new(t),
                        None => CompactStr::new(name),
                    };

                    nodes.push(ContractNode {
                        id: 0,
                        name: final_name,
                        kind,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                }
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

    fn first_line(node: Node, source: &[u8], fallback: &str) -> String {
        node.utf8_text(source)
            .ok()
            .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
            .unwrap_or_else(|| fallback.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_kotlin_ng::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_kotlin_extractor_spring_controller() {
        let code = r#"
package com.mesh.billing

@RestController
class BillingController {
    @GetMapping("/bills")
    fun getBills(): List<Bill> {
        return emptyList()
    }

    @KafkaListener(topics = "billing.events")
    fun onEvent(msg: String) {
    }
}

interface BillingGateway {
    fun charge(amount: Int)
}
"#;
        let mut p = parser();
        let nodes = KotlinExtractor::extract(Path::new("BillingController.kt"), code, 1, &mut p);

        let find = |name: &str| nodes.iter().find(|n| n.name == name);

        assert!(find("BillingController").is_some());
        assert_eq!(
            find("getBills").expect("http endpoint").kind,
            NodeKind::HttpEndpoint
        );
        assert_eq!(
            find("billing.events").expect("kafka topic").kind,
            NodeKind::KafkaTopic
        );
        assert_eq!(
            find("BillingGateway").expect("interface").kind,
            NodeKind::Interface
        );
        assert!(nodes.iter().all(|n| n.repo_id == 1));
        assert_eq!(
            find("BillingController").unwrap().package.as_str(),
            "com.mesh.billing"
        );
    }

    #[test]
    fn test_kotlin_extractor_object_declaration() {
        let code = r#"
package com.mesh.util

object Constants {
    const val VERSION = "1.0"
}
"#;
        let mut p = parser();
        let nodes = KotlinExtractor::extract(Path::new("Constants.kt"), code, 0, &mut p);
        let constants = nodes
            .iter()
            .find(|n| n.name == "Constants")
            .expect("object");
        assert_eq!(constants.kind, NodeKind::ServiceClass);
    }
}
