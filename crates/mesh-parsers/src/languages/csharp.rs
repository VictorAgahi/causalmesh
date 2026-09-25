use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Default)]
pub struct CSharpRelations {
    pub producers: Vec<(usize, CompactStr)>,
    pub consumers: Vec<(usize, CompactStr)>,
}

#[derive(Debug)]
struct RawKafkaCall {
    line_start: usize,
    line_end: usize,
    topic: CompactStr,
    is_producer: bool,
}

pub struct CSharpExtractor;

impl CSharpExtractor {
    /// Test/ad-hoc entry point: parses `content` itself. Production indexing goes
    /// through [`Self::extract_with_relations`] via `PolyglotIndexer`, which parses
    /// once with `AstGuard::parse_with` so a parse failure is visible instead of
    /// silently producing an empty result indistinguishable from a legitimately
    /// empty file.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        let Some(tree) = parser.parse(content, None) else {
            return Vec::new();
        };
        Self::extract_with_relations(file_path, content, repo_id, &tree).0
    }

    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> (Vec<ContractNode>, CSharpRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut relations = CSharpRelations::default();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = mesh_core::detect_service_package(&file_path, None);
        let mut raw_kafka_calls = Vec::new();

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
            &mut raw_kafka_calls,
            0,
        );
        Self::resolve_kafka_calls(&nodes, raw_kafka_calls, &mut relations);

        (nodes, relations)
    }

    fn resolve_kafka_calls(
        nodes: &[ContractNode],
        raw_calls: Vec<RawKafkaCall>,
        relations: &mut CSharpRelations,
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

    /// Only a genuine string literal (or a `TopicPartition("...")` wrapping
    /// one) counts as a topic. A bare `identifier` argument used to be
    /// emitted verbatim as the "topic" — e.g. `producer.Produce(topic, ...)`
    /// recorded the literal string `"topic"`, which is the variable's name,
    /// not its value, and this extractor has no constant-propagation pass to
    /// resolve it against (unlike Kotlin's `string_defaults`).
    fn extract_topic_arg(args_node: Node, source: &[u8]) -> Option<CompactStr> {
        let mut cursor = args_node.walk();
        for child in args_node.named_children(&mut cursor) {
            let expr = if child.kind() == "argument" {
                child.named_child(0).unwrap_or(child)
            } else {
                child
            };
            if expr.kind() == "string_literal" {
                if let Ok(text) = expr.utf8_text(source) {
                    let unquoted = text.trim_matches('"');
                    if !unquoted.is_empty() {
                        return Some(CompactStr::new(unquoted));
                    }
                }
            }
            if expr.kind() == "object_creation_expression" {
                let type_node = expr
                    .child_by_field_name("type")
                    .or_else(|| expr.named_child(0));
                if let Some(t) = type_node.and_then(|t| t.utf8_text(source).ok()) {
                    if t.ends_with("TopicPartition") {
                        let inner_args = expr
                            .child_by_field_name("arguments")
                            .or_else(|| expr.child_by_field_name("argument_list"))
                            .or_else(|| expr.named_child(1));
                        if let Some(args) = inner_args {
                            if let Some(first_arg) = args.named_child(0) {
                                return Self::extract_single_topic_arg(first_arg, source);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn extract_single_topic_arg(node: Node, source: &[u8]) -> Option<CompactStr> {
        let unwrapped = if node.kind() == "argument" {
            node.named_child(0).unwrap_or(node)
        } else {
            node
        };
        if unwrapped.kind() == "string_literal" {
            let text = unwrapped.utf8_text(source).ok()?;
            let unquoted = text.trim_matches('"');
            if !unquoted.is_empty() {
                return Some(CompactStr::new(unquoted));
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
        raw_kafka_calls: &mut Vec<RawKafkaCall>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        match node.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    *package_name = mesh_core::detect_service_package(file_path, Some(name));
                }
            }
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let first_line = Self::first_line(node, source, name);

                    let kind = if node.kind() == "interface_declaration" {
                        NodeKind::Interface
                    } else {
                        // ASP.NET controllers ([ApiController]/[Controller]) and plain
                        // classes both surface as ServiceClass; the attribute is kept
                        // in the signature line rather than a separate NodeKind, since
                        // MeshMCP has no `Controller` variant of its own.
                        NodeKind::ServiceClass
                    };

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
            "method_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let attrs = Self::attribute_lists_text(node, source);

                    let kind = if attrs.contains("[HttpGet")
                        || attrs.contains("[HttpPost")
                        || attrs.contains("[HttpPut")
                        || attrs.contains("[HttpDelete")
                        || attrs.contains("[HttpPatch")
                        || attrs.contains("[Route")
                    {
                        NodeKind::HttpEndpoint
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
            "invocation_expression" => {
                let func_node = node
                    .child_by_field_name("function")
                    .or_else(|| node.named_child(0));
                let member_name = func_node
                    .and_then(|f| {
                        if f.kind() == "member_access_expression" {
                            f.child_by_field_name("name").or_else(|| f.named_child(1))
                        } else {
                            None
                        }
                    })
                    .and_then(|n| n.utf8_text(source).ok());

                let is_producer = match member_name {
                    Some("Produce" | "ProduceAsync") => Some(true),
                    Some("Subscribe") => Some(false),
                    _ => None,
                };

                if let Some(is_producer) = is_producer {
                    let args_node = node
                        .child_by_field_name("arguments")
                        .or_else(|| node.child_by_field_name("argument_list"))
                        .or_else(|| node.named_child(1));
                    if let Some(args) = args_node {
                        if let Some(topic) = Self::extract_topic_arg(args, source) {
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
                raw_kafka_calls,
                depth + 1,
            );
        }
    }

    /// Attribute lists (`[ApiController]`, `[HttpGet("/x")]`) are direct children of the
    /// declaration node in the C# grammar (siblings of `name`/`body`, not wrapped inside
    /// either), so this concatenates every `attribute_list` child's text.
    fn attribute_lists_text(node: Node, source: &[u8]) -> String {
        let mut text = String::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "attribute_list" {
                if let Ok(t) = child.utf8_text(source) {
                    text.push_str(t);
                    text.push(' ');
                }
            }
        }
        text
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
        let lang = tree_sitter_c_sharp::language();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_csharp_extractor_controller_and_interface() {
        let code = r#"
namespace Mesh.Billing;

public interface IBillingService
{
    Task<Bill> GetBill(int id);
}

[ApiController]
[Route("api/[controller]")]
public class BillingController : ControllerBase
{
    [HttpGet("/bills")]
    public IActionResult GetBills()
    {
        return Ok();
    }

    [HttpPost("/bills")]
    public IActionResult CreateBill()
    {
        return Ok();
    }
}
"#;
        let mut p = parser();
        let nodes = CSharpExtractor::extract(
            Path::new("services/billing/BillingController.cs"),
            code,
            2,
            &mut p,
        );

        let find = |name: &str| nodes.iter().find(|n| n.name == name);

        assert_eq!(
            find("IBillingService").expect("interface").kind,
            NodeKind::Interface
        );
        assert_eq!(
            find("BillingController").expect("controller class").kind,
            NodeKind::ServiceClass
        );
        assert_eq!(
            find("GetBills").expect("http get").kind,
            NodeKind::HttpEndpoint
        );
        assert_eq!(
            find("CreateBill").expect("http post").kind,
            NodeKind::HttpEndpoint
        );
        assert!(nodes.iter().all(|n| n.repo_id == 2));
        assert_eq!(
            find("BillingController").unwrap().package.as_str(),
            "Mesh.Billing"
        );
    }

    #[test]
    fn test_csharp_extractor_plain_method_is_service_class() {
        let code = r#"
namespace Mesh.Util;

public class MathHelper
{
    public int Add(int a, int b)
    {
        return a + b;
    }
}
"#;
        let mut p = parser();
        let nodes = CSharpExtractor::extract(Path::new("MathHelper.cs"), code, 0, &mut p);
        let add = nodes.iter().find(|n| n.name == "Add").expect("method");
        assert_eq!(add.kind, NodeKind::ServiceClass);
    }

    #[test]
    fn test_csharp_confluent_kafka_produce_and_subscribe_extraction() {
        let code = r#"
namespace Mesh.Orders;

public class OrderEventService
{
    public async Task EmitOrderCreated(Order order)
    {
        await _producer.ProduceAsync("orders-created", new Message<string, string> { Value = order.Id });
    }

    public void StartListening()
    {
        _consumer.Subscribe("orders-incoming");
    }
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (nodes, relations) = CSharpExtractor::extract_with_relations(
            Path::new("OrderEventService.cs"),
            code,
            0,
            &tree,
        );

        assert_eq!(nodes.len(), 3); // Class + 2 methods
        assert_eq!(
            relations.producers.len(),
            1,
            "expected 1 producer, got: {:?}",
            relations.producers
        );
        assert_eq!(relations.producers[0].1.as_str(), "orders-created");
        assert_eq!(
            nodes[relations.producers[0].0].name.as_str(),
            "EmitOrderCreated"
        );

        assert_eq!(
            relations.consumers.len(),
            1,
            "expected 1 consumer, got: {:?}",
            relations.consumers
        );
        assert_eq!(relations.consumers[0].1.as_str(), "orders-incoming");
        assert_eq!(
            nodes[relations.consumers[0].0].name.as_str(),
            "StartListening"
        );
    }

    /// A `Produce`/`Subscribe` call whose topic argument is a variable, not a
    /// string literal, must not record any topic at all. It used to fall
    /// back to the identifier's own name (`topic` on `_producer.Produce(topic,
    /// ...)`), fabricating a "topic" that was really the variable's name —
    /// this extractor has no constant-propagation pass to resolve it against.
    #[test]
    fn kafka_call_with_identifier_topic_records_nothing() {
        let code = r#"
namespace Mesh.Orders;

public class OrderEventService
{
    public async Task EmitOrderCreated(string topic, Order order)
    {
        await _producer.ProduceAsync(topic, new Message<string, string> { Value = order.Id });
    }
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) = CSharpExtractor::extract_with_relations(
            Path::new("OrderEventService.cs"),
            code,
            0,
            &tree,
        );
        assert!(
            relations.producers.is_empty(),
            "expected no fabricated topic from an identifier argument, got: {:?}",
            relations.producers
        );
    }
}
