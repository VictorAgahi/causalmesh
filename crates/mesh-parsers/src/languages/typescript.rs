use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct TypeScriptExtractor;

/// Historical hardcoded gRPC decorator, used when `controller_annotations`
/// is not configured.
const DEFAULT_GRPC_ANNOTATION: &str = "@GrpcMethod";

/// Per-file invariants threaded through the recursive `visit_node` walk,
/// bundled to keep its argument count down.
struct VisitCtx<'a> {
    file_path: &'a FilePath,
    repo_id: RepoId,
    package_name: &'a CompactStr,
    grpc_annotations: &'a [String],
}

impl TypeScriptExtractor {
    /// Extracts using the legacy hardcoded `@GrpcMethod` decorator.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
    ) -> Vec<ContractNode> {
        Self::extract_with_config(
            file_path,
            content,
            repo_id,
            parser,
            imports,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        )
    }

    /// Same as [`Self::extract`], but `grpc_annotations` (from
    /// `[engines.contracts.grpc] controller_annotations`) replaces the
    /// hardcoded `@GrpcMethod` decorator list used to recognise gRPC handlers.
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
        grpc_annotations: &[String],
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return nodes,
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);
        let ctx = VisitCtx {
            file_path: &file_path,
            repo_id,
            package_name: &package_name,
            grpc_annotations,
        };

        Self::visit_node(root, source_bytes, &ctx, &mut nodes, imports, 0);
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        ctx: &VisitCtx,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        let file_path = ctx.file_path;
        let repo_id = ctx.repo_id;
        let package_name = ctx.package_name;
        let grpc_annotations = ctx.grpc_annotations;
        match node.kind() {
            "import_statement" => {
                if let Ok(text) = node.utf8_text(source) {
                    if let Some(from_idx) = text.find("from") {
                        let from_str = text[from_idx + 4..]
                            .trim()
                            .trim_matches(';')
                            .trim()
                            .trim_matches('\'')
                            .trim_matches('"');

                        imports.push((String::new(), from_str.to_string()));

                        for named in Self::collect_named_import_specifiers(node, source) {
                            imports.push((String::new(), named));
                        }
                    }
                }
            }
            "class_declaration" | "interface_declaration" => {
                let class_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownClass");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("class {class_name}"));

                let kind = if node.kind() == "interface_declaration" {
                    NodeKind::Interface
                } else {
                    NodeKind::ServiceClass
                };

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(class_name),
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
            "method_definition" => {
                let method_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownMethod");

                let mut kind = NodeKind::ServiceClass;
                let mut grpc_target = None;

                let mut full_text = String::new();
                if let Some(prev) = node.prev_sibling() {
                    if prev.kind() == "decorator" {
                        if let Ok(t) = prev.utf8_text(source) {
                            full_text.push_str(t);
                            full_text.push(' ');
                        }
                    }
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "decorator" {
                        if let Ok(t) = child.utf8_text(source) {
                            full_text.push_str(t);
                            full_text.push(' ');
                        }
                    }
                }
                if let Ok(t) = node.utf8_text(source) {
                    full_text.push_str(t);
                }

                let matched_annotation = grpc_annotations
                    .iter()
                    .find(|a| full_text.contains(a.as_str()));
                if let Some(annotation) = matched_annotation {
                    kind = NodeKind::GrpcMethod;
                    grpc_target = Self::extract_grpc_method(&full_text, annotation);
                } else if full_text.contains("@Get")
                    || full_text.contains("@Post")
                    || full_text.contains("@Put")
                {
                    kind = NodeKind::HttpEndpoint;
                } else if full_text.contains("@EventPattern") {
                    // @nestjs/microservices event handler: a message-bus consumer.
                    kind = NodeKind::KafkaTopic;
                    grpc_target = Self::extract_decorator_single_arg(&full_text, "@EventPattern");
                } else if full_text.contains("@MessagePattern") {
                    // @nestjs/microservices RPC-style handler: also a consumer side.
                    kind = NodeKind::KafkaTopic;
                    grpc_target = Self::extract_decorator_single_arg(&full_text, "@MessagePattern");
                }

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("{method_name}()"));

                let final_name = if let Some(g) = grpc_target {
                    CompactStr::new(g)
                } else {
                    CompactStr::new(method_name)
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
            // kafkajs: `consumer.subscribe({ topic: 'x' })` / `producer.send({ topic: 'x' })`.
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    if callee.kind() == "member_expression" {
                        if let Some(prop) = callee.child_by_field_name("property") {
                            if let Ok(prop_name) = prop.utf8_text(source) {
                                let signature = match prop_name {
                                    "subscribe" => Some("kafkajs consumer.subscribe"),
                                    "send" => Some("kafkajs producer.send"),
                                    _ => None,
                                };
                                if let Some(signature) = signature {
                                    if let Some(args) = node.child_by_field_name("arguments") {
                                        if let Some(topic) =
                                            Self::extract_object_value(args, source, "topic")
                                        {
                                            nodes.push(ContractNode {
                                                id: 0,
                                                name: CompactStr::new(topic),
                                                kind: NodeKind::KafkaTopic,
                                                file_path: file_path.clone(),
                                                line_start: node.start_position().row + 1,
                                                line_end: node.end_position().row + 1,
                                                package: package_name.clone(),
                                                repo_id,
                                                signature: Some(CompactStr::new(signature)),
                                                docstring: None,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // BullMQ: `new Queue('name')` is a producer-side topic/queue declaration.
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    if ctor.kind() == "identifier" {
                        if let Ok("Queue") = ctor.utf8_text(source) {
                            if let Some(args) = node.child_by_field_name("arguments") {
                                if let Some(queue_name) =
                                    Self::extract_first_arg_value(args, source)
                                {
                                    nodes.push(ContractNode {
                                        id: 0,
                                        name: CompactStr::new(queue_name),
                                        kind: NodeKind::Queue,
                                        file_path: file_path.clone(),
                                        line_start: node.start_position().row + 1,
                                        line_end: node.end_position().row + 1,
                                        package: package_name.clone(),
                                        repo_id,
                                        signature: Some(CompactStr::new("bullmq new Queue()")),
                                        docstring: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, ctx, nodes, imports, depth + 1);
        }
    }

    /// Walks an `import_statement` node's `import_clause` -> `named_imports`
    /// children to collect each individually named specifier's *original*
    /// exported identifier (the `name` field — never the local `alias`,
    /// since matching needs the name as declared in the source file, not
    /// however the importer chose to rebind it locally).
    fn collect_named_import_specifiers<'a>(node: Node<'a>, source: &'a [u8]) -> Vec<String> {
        let mut specifiers = Vec::new();

        for clause in node.children(&mut node.walk()) {
            if clause.kind() != "import_clause" {
                continue;
            }
            for part in clause.children(&mut clause.walk()) {
                if part.kind() != "named_imports" {
                    continue;
                }
                for spec in part.children(&mut part.walk()) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    if let Some(name_node) = spec.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(source) {
                            specifiers.push(name.to_string());
                        }
                    }
                }
            }
        }

        specifiers
    }

    fn extract_grpc_method(text: &str, annotation: &str) -> Option<String> {
        if let Some(idx) = text.find(annotation) {
            let rest = &text[idx + annotation.len()..];
            if let Some(paren_open) = rest.find('(') {
                if let Some(paren_close) = rest[paren_open + 1..].find(')') {
                    let args = &rest[paren_open + 1..paren_open + 1 + paren_close];
                    let parts: Vec<&str> = args
                        .split(',')
                        .map(|s| {
                            let trimmed = s.trim();
                            if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
                                || (trimmed.starts_with('"') && trimmed.ends_with('"'))
                                || (trimmed.starts_with('`') && trimmed.ends_with('`'))
                            {
                                trimmed
                                    .trim_matches('\'')
                                    .trim_matches('"')
                                    .trim_matches('`')
                            } else if let Some(last_dot) = trimmed.rfind('.') {
                                &trimmed[last_dot + 1..]
                            } else {
                                trimmed
                            }
                        })
                        .filter(|s| !s.is_empty())
                        .collect();
                    if parts.len() == 2 {
                        return Some(format!("{}.{}", parts[0], parts[1]));
                    } else if let Some(first) = parts.first() {
                        return Some(first.to_string());
                    }
                }
            }
        }
        None
    }

    /// Extracts a single-argument decorator's value, e.g. `@EventPattern('order.created')`
    /// or `@MessagePattern(ORDER_CMD)`. Quoted literals are unquoted; anything else
    /// (a variable, enum member, or config lookup) is kept verbatim rather than
    /// dropped, so callers still see that *some* topic/pattern was referenced there.
    fn extract_decorator_single_arg(text: &str, decorator: &str) -> Option<String> {
        let idx = text.find(decorator)?;
        let rest = &text[idx + decorator.len()..];
        let paren_open = rest.find('(')?;
        let paren_close = rest[paren_open + 1..].find(')')?;
        let arg = rest[paren_open + 1..paren_open + 1 + paren_close].trim();
        let first = arg.split(',').next().unwrap_or(arg).trim();
        if first.is_empty() {
            return None;
        }
        Some(
            first
                .trim_matches('\'')
                .trim_matches('"')
                .trim_matches('`')
                .to_string(),
        )
    }

    /// Looks for an object literal among `args` (a call's `arguments` node) and
    /// returns the value of its `key` property, e.g. `topic` in
    /// `{ topic: 'orders' }`. Falls through non-object arguments untouched.
    fn extract_object_value(args: Node, source: &[u8], key: &str) -> Option<String> {
        let mut cursor = args.walk();
        for arg in args.children(&mut cursor) {
            if arg.kind() != "object" {
                continue;
            }
            let mut pair_cursor = arg.walk();
            for pair in arg.children(&mut pair_cursor) {
                if pair.kind() != "pair" {
                    continue;
                }
                let Some(key_node) = pair.child_by_field_name("key") else {
                    continue;
                };
                let Ok(key_text) = key_node.utf8_text(source) else {
                    continue;
                };
                if key_text.trim_matches('\'').trim_matches('"') != key {
                    continue;
                }
                if let Some(value_node) = pair.child_by_field_name("value") {
                    return Self::extract_value_text(value_node, source);
                }
            }
        }
        None
    }

    /// Returns the first named argument of a call's `arguments` node, e.g. the
    /// `'email-queue'` in `new Queue('email-queue')`.
    fn extract_first_arg_value(args: Node, source: &[u8]) -> Option<String> {
        let mut cursor = args.walk();
        for arg in args.children(&mut cursor) {
            if arg.is_named() {
                return Self::extract_value_text(arg, source);
            }
        }
        None
    }

    /// Unquotes a string-literal node's text; for anything else (identifier,
    /// member expression, ...) returns the raw expression text — this is the
    /// "not a literal" escape hatch so a variable/config-lookup topic name is
    /// still surfaced instead of silently dropped.
    fn extract_value_text(value_node: Node, source: &[u8]) -> Option<String> {
        let text = value_node.utf8_text(source).ok()?;
        if value_node.kind() == "string" {
            Some(
                text.trim_matches('\'')
                    .trim_matches('"')
                    .trim_matches('`')
                    .to_string(),
            )
        } else {
            Some(text.trim().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{ContractGraph, EdgeKind};

    fn parse(code: &str) -> (Vec<ContractNode>, Vec<(String, String)>) {
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let nodes = TypeScriptExtractor::extract(
            Path::new("events.ts"),
            code,
            1,
            &mut parser,
            &mut imports,
        );
        (nodes, imports)
    }

    #[test]
    fn test_ts_extractor() {
        let code = r#"
import { UserAuthRequest } from '@volontariapp/domain-user';

@Controller('auth')
export class AuthController {
    @GrpcMethod('AuthService', 'AuthenticateUser')
    async authenticateUser(data: any): Promise<any> {
        return null;
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();

        let mut imports = Vec::new();
        let nodes = TypeScriptExtractor::extract(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports,
        );

        // One module-path-level entry plus one entry per named specifier
        // (here just `UserAuthRequest`).
        assert_eq!(imports.len(), 2);
        assert!(imports
            .iter()
            .any(|(_, t)| t == "@volontariapp/domain-user"));
        assert!(imports.iter().any(|(_, t)| t == "UserAuthRequest"));
        assert!(nodes.iter().any(|n| n.name == "AuthController"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser" && n.kind == NodeKind::GrpcMethod));
    }

    #[test]
    fn test_ts_extractor_configurable_controller_annotations() {
        let code = r#"
@Controller('auth')
export class AuthController {
    @RpcHandler('AuthService', 'AuthenticateUser')
    async authenticateUser(data: any): Promise<any> {
        return null;
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();

        // Default decorator list (`@GrpcMethod`) does not recognise `@RpcHandler`.
        let default_nodes = TypeScriptExtractor::extract(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports,
        );
        assert!(!default_nodes.iter().any(|n| n.kind == NodeKind::GrpcMethod));

        // A configured `controller_annotations` list must produce an
        // observably different result: the method is now recognised as a
        // gRPC handler and projected via the same `Service.Method` parsing.
        let mut imports2 = Vec::new();
        let configured_nodes = TypeScriptExtractor::extract_with_config(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports2,
            &["@RpcHandler".to_string()],
        );
        assert!(configured_nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser" && n.kind == NodeKind::GrpcMethod));
    }

    #[test]
    fn test_ts_extractor_grpc_method_enum_identifiers() {
        let code = r#"
@Controller('user')
export class UserCommandController {
    @GrpcMethod(USER_SERVICE_NAME, USER_COMMAND_METHODS.SIGN_UP)
    async signUp(data: SignUpCommandDTO): Promise<SignUpResponseDTO> {
        return null;
    }

    @GrpcMethod(GRPC_SERVICES.EVENT_COMMAND_SERVICE, UserCommandMethod.CREATE_EVENT)
    async createEvent(data: any): Promise<any> {
        return null;
    }
}
"#;
        let (nodes, _) = parse(code);
        assert!(nodes
            .iter()
            .any(|n| n.name == "USER_SERVICE_NAME.SIGN_UP" && n.kind == NodeKind::GrpcMethod));
        assert!(nodes
            .iter()
            .any(|n| n.name == "EVENT_COMMAND_SERVICE.CREATE_EVENT"
                && n.kind == NodeKind::GrpcMethod));
    }

    #[test]
    fn test_kafkajs_consumer_and_producer_literal_topic() {
        let code = r#"
async function run() {
    await consumer.subscribe({ topic: 'orders', fromBeginning: true });
    await producer.send({ topic: 'orders', messages: [] });
}
"#;
        let (nodes, _) = parse(code);

        assert!(nodes.iter().any(|n| {
            n.name == "orders"
                && n.kind == NodeKind::KafkaTopic
                && n.signature.as_deref() == Some("kafkajs consumer.subscribe")
        }));
        assert!(nodes.iter().any(|n| {
            n.name == "orders"
                && n.kind == NodeKind::KafkaTopic
                && n.signature.as_deref() == Some("kafkajs producer.send")
        }));
    }

    #[test]
    fn test_kafkajs_non_literal_topic_emits_variable_name() {
        let code = r#"
async function run() {
    await consumer.subscribe({ topic: TOPIC_NAME });
}
"#;
        let (nodes, _) = parse(code);

        assert!(nodes
            .iter()
            .any(|n| n.name == "TOPIC_NAME" && n.kind == NodeKind::KafkaTopic));
    }

    #[test]
    fn test_nestjs_event_and_message_pattern_decorators() {
        let code = r#"
@Controller()
export class OrdersController {
    @EventPattern('order.created')
    handleOrderCreated(data: any) {}

    @MessagePattern('order.get')
    handleGetOrder(data: any) {}
}
"#;
        let (nodes, _) = parse(code);

        assert!(nodes
            .iter()
            .any(|n| n.name == "order.created" && n.kind == NodeKind::KafkaTopic));
        assert!(nodes
            .iter()
            .any(|n| n.name == "order.get" && n.kind == NodeKind::KafkaTopic));
    }

    #[test]
    fn test_bullmq_new_queue_declaration() {
        let code = r#"
const emailQueue = new Queue('email-queue');
"#;
        let (nodes, _) = parse(code);

        assert!(nodes
            .iter()
            .any(|n| n.name == "email-queue" && n.kind == NodeKind::Queue));
    }

    #[test]
    fn test_bullmq_new_queue_non_literal_name() {
        let code = r#"
const emailQueue = new Queue(QUEUE_NAME);
"#;
        let (nodes, _) = parse(code);

        assert!(nodes
            .iter()
            .any(|n| n.name == "QUEUE_NAME" && n.kind == NodeKind::Queue));
    }

    /// Definition-of-done test: a kafkajs producer and consumer for the same
    /// topic, detected with zero custom `[[engines.contracts.patterns]]`
    /// configuration, are linked by `ContractGraph::analyze_impact` — the
    /// producer feeds a `Produces` edge and the consumer a `Consumes` edge
    /// into the same topic hub, and reconciliation derives a direct
    /// `DispatchesTo` causal edge between them.
    #[test]
    fn test_kafkajs_producer_consumer_linked_via_analyze_impact() {
        let code = r#"
async function run() {
    await consumer.subscribe({ topic: 'orders' });
    await producer.send({ topic: 'orders' });
}
"#;
        let (nodes, _) = parse(code);

        let consumer_node = nodes
            .iter()
            .find(|n| n.signature.as_deref() == Some("kafkajs consumer.subscribe"))
            .expect("kafkajs consumer.subscribe node not detected")
            .clone();
        let producer_node = nodes
            .iter()
            .find(|n| n.signature.as_deref() == Some("kafkajs producer.send"))
            .expect("kafkajs producer.send node not detected")
            .clone();

        // Mirrors how `PolyglotIndexer` wires a language extractor's tagged
        // nodes into `FileIndex.producers`/`.consumers` (see
        // `languages/mod.rs`'s Java branch) — done inline here since this
        // extractor's own scope doesn't include that dispatch wiring.
        let mut graph = ContractGraph::new();
        let consumer_topic = consumer_node.name.clone();
        let producer_topic = producer_node.name.clone();
        let consumer_id = graph.add_node(consumer_node);
        let producer_id = graph.add_node(producer_node);
        graph.add_consumer(consumer_id, &consumer_topic);
        graph.add_producer(producer_id, &producer_topic);
        graph.reconcile_edges();

        let impact = graph.analyze_impact("orders");
        assert!(
            !impact.upstream_producers.is_empty(),
            "expected the kafkajs producer to be linked as an upstream producer"
        );
        assert!(
            !impact.downstream_consumers.is_empty(),
            "expected the kafkajs consumer to be linked as a downstream consumer"
        );
        // ROADMAP #11 dropped direct producer->consumer `DispatchesTo` edges (they
        // were O(producers * consumers) per topic); the same information is now
        // carried by the two-hop `Produces`/`Consumes` walk through the topic,
        // which `analyze_impact` already reads directly from the topic registry
        // above — not by materializing a direct edge here.
        assert!(
            graph
                .all_edges()
                .iter()
                .any(|e| e.kind == EdgeKind::Produces && e.from == producer_id),
            "expected a Produces edge from the kafkajs producer to the topic"
        );
        assert!(
            graph
                .all_edges()
                .iter()
                .any(|e| e.kind == EdgeKind::Consumes && e.to == consumer_id),
            "expected a Consumes edge from the topic to the kafkajs consumer"
        );
    }
}
