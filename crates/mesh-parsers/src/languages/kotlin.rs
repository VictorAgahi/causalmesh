use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

/// Native (non-`@KafkaListener`) `kafka-clients` producer/consumer relations,
/// keyed the same way `languages::FileIndex.producers`/`.consumers` expect
/// (local node index, topic) — see `languages/mod.rs`'s `LanguageKind::Kotlin`
/// branch.
#[derive(Debug, Default)]
pub struct KotlinRelations {
    pub producers: Vec<(usize, CompactStr)>,
    pub consumers: Vec<(usize, CompactStr)>,
}

/// A raw `.subscribe(...)`/`.send(...)`/`.produce(...)` call on a
/// Kafka-shaped receiver, found at `(line_start, line_end)`. Resolved to its
/// smallest enclosing declaration in `resolve_kafka_calls`, mirroring
/// `go.rs`'s `RawEvent`/`resolve_events`.
struct RawKafkaCall {
    line_start: usize,
    line_end: usize,
    topic: CompactStr,
    is_producer: bool,
}

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
        Self::extract_with_relations(file_path, content, repo_id, parser).0
    }

    /// Same extraction as [`Self::extract`], plus native `kafka-clients`
    /// producer/consumer relations (a `.subscribe(...)`/`.send(...)` call,
    /// as opposed to the Spring `@KafkaListener` annotation handled inline
    /// in `visit_node`'s `function_declaration` arm).
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> (Vec<ContractNode>, KotlinRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut relations = KotlinRelations::default();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return (nodes, relations),
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = mesh_core::detect_service_package(&file_path, None);

        // Pre-pass: `val`/`var` declarations whose initializer is an elvis
        // (`?:`) expression with a string-literal right operand — e.g.
        // `val topic = System.getenv("KAFKA_TOPIC") ?: "orders"` — so a
        // later `.subscribe(topic)` call referencing `topic` by name can
        // resolve to the real default topic instead of the bare identifier.
        // Deliberately bounded, same non-goal as Go's `getTopic()` function
        // case: only a direct `= ... ?: "literal"` shape resolves.
        let mut string_defaults: HashMap<String, CompactStr> = HashMap::new();
        Self::collect_elvis_string_defaults(root, source_bytes, &mut string_defaults, 0);

        let mut raw_kafka_calls: Vec<RawKafkaCall> = Vec::new();
        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
            &string_defaults,
            &mut raw_kafka_calls,
            0,
        );
        Self::resolve_kafka_calls(&nodes, raw_kafka_calls, &mut relations);

        (nodes, relations)
    }

    fn direct_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        let found = node.children(&mut cursor).find(|c| c.kind() == kind);
        found
    }

    /// Pre-pass collecting `val`/`var X = ... ?: "literal"` elvis-default
    /// string assignments (any scope), keyed on the bare identifier.
    fn collect_elvis_string_defaults(
        node: Node,
        source: &[u8],
        out: &mut HashMap<String, CompactStr>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "property_declaration" {
            let name = node
                .named_child(0)
                .filter(|n| n.kind() == "variable_declaration")
                .and_then(|vd| vd.named_child(0))
                .and_then(|id| id.utf8_text(source).ok());
            let elvis_default =
                Self::direct_child_of_kind(node, "binary_expression").and_then(|bin| {
                    let right = bin.child_by_field_name("right")?;
                    if right.kind() == "string_literal" {
                        right.utf8_text(source).ok()
                    } else {
                        None
                    }
                });
            if let (Some(name), Some(value)) = (name, elvis_default) {
                let unquoted = value.trim_matches('"');
                if !unquoted.is_empty() {
                    out.insert(name.to_string(), CompactStr::new(unquoted));
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_elvis_string_defaults(child, source, out, depth + 1);
        }
    }

    /// Attaches each raw Kafka call to its smallest enclosing declaration,
    /// mirroring `go.rs::resolve_events`. A call with no enclosing
    /// declaration has no sensible owner to attribute it to, so it is
    /// dropped rather than given a synthetic node (unlike Go's Kafka
    /// handling, which does synthesize one — kept simpler here since this is
    /// new coverage, not a behavior-preserving port).
    fn resolve_kafka_calls(
        nodes: &[ContractNode],
        raw_calls: Vec<RawKafkaCall>,
        relations: &mut KotlinRelations,
    ) {
        for call in raw_calls {
            let enclosing = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.line_start <= call.line_start && n.line_end >= call.line_end)
                .min_by_key(|(_, n)| n.line_end - n.line_start);
            if let Some((idx, _)) = enclosing {
                if call.is_producer {
                    relations.producers.push((idx, call.topic));
                } else {
                    relations.consumers.push((idx, call.topic));
                }
            }
        }
    }

    /// Finds the first string literal among `node`'s descendants (bounded
    /// depth); if none, falls back to the first identifier. Mirrors Go's
    /// `collect_string_literals` "literal first, identifier fallback"
    /// approach. Used to look inside a `.subscribe(listOf(topic))`-style
    /// wrapped call for the real argument without needing to model every
    /// possible wrapper function.
    fn find_topic_arg(node: Node, source: &[u8]) -> Option<CompactStr> {
        let mut literals = Vec::new();
        Self::collect_kinds(node, source, "string_literal", &mut literals, 0);
        if let Some(first) = literals.into_iter().next() {
            let unquoted = first.trim_matches('"');
            if !unquoted.is_empty() {
                return Some(CompactStr::new(unquoted));
            }
        }
        let mut idents = Vec::new();
        Self::collect_kinds(node, source, "identifier", &mut idents, 0);
        idents.into_iter().next().map(|s| CompactStr::new(s))
    }

    fn collect_kinds(node: Node, source: &[u8], kind: &str, out: &mut Vec<String>, depth: usize) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == kind {
            if let Ok(text) = node.utf8_text(source) {
                out.push(text.to_string());
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            Self::collect_kinds(child, source, kind, out, depth + 1);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
        string_defaults: &HashMap<String, CompactStr>,
        raw_kafka_calls: &mut Vec<RawKafkaCall>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

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
            // Native `org.apache.kafka.clients` producer/consumer detection:
            // `consumer.subscribe(...)` / `producer.send(...)` /
            // `producer.produce(...)`, as opposed to the Spring
            // `@KafkaListener` annotation handled above. The real-world
            // shape (`consumer.subscribe(listOf(topic))`) wraps its argument
            // in `listOf(...)`, so the topic is looked up via
            // `find_topic_arg`'s bounded recursive search rather than a
            // single direct child access.
            "call_expression" => {
                let method_name = node
                    .named_child(0)
                    .filter(|f| f.kind() == "navigation_expression")
                    .and_then(|nav| nav.named_child(1))
                    .and_then(|n| n.utf8_text(source).ok());
                let is_producer = match method_name {
                    Some("send") | Some("produce") => Some(true),
                    Some("subscribe") => Some(false),
                    _ => None,
                };
                if let Some(is_producer) = is_producer {
                    if let Some(args) = node.named_child(1) {
                        if let Some(topic_text) = Self::find_topic_arg(args, source) {
                            let topic = string_defaults
                                .get(topic_text.as_str())
                                .cloned()
                                .unwrap_or(topic_text);
                            raw_kafka_calls.push(RawKafkaCall {
                                line_start: node.start_position().row + 1,
                                line_end: node.end_position().row + 1,
                                topic,
                                is_producer,
                            });
                        }
                    }
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(
                child,
                source,
                file_path,
                repo_id,
                package_name,
                nodes,
                string_defaults,
                raw_kafka_calls,
                depth + 1,
            );
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
