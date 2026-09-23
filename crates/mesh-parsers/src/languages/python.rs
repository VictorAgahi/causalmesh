use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

/// A single native import reference found while walking the tree.
///
/// `target` is the name pushed as the dependency's edge target (the name as
/// declared at the source, e.g. the original symbol for an aliased import).
/// `search_text` is what is looked up in a node's body text to decide whether
/// that specific node actually uses the import (the local alias when one is
/// present, since that is what the code underneath the import actually
/// references). `module_like` mirrors the TypeScript extractor's heuristic
/// (`languages/mod.rs` ~185-205): a relative import (`from .x import y`) or a
/// wildcard import cannot be reliably matched against a body substring, so it
/// is treated as used everywhere in the file rather than silently dropped.
struct ImportRef {
    target: String,
    search_text: String,
    module_like: bool,
}

/// Native import and event relations extracted alongside the node list,
/// keyed by the *local* index into the sibling `Vec<ContractNode>` returned
/// from the same call — the same shape `languages/mod.rs::FileIndex` uses for
/// `.dependencies` / `.producers` / `.consumers`, so wiring this into the
/// `PolyglotIndexer::extract` Python branch is a direct `extend`.
#[derive(Debug, Default)]
pub struct PythonRelations {
    pub dependencies: Vec<(usize, CompactStr)>,
    pub producers: Vec<(usize, CompactStr)>,
    pub consumers: Vec<(usize, CompactStr)>,
}

pub struct PythonExtractor;

impl PythonExtractor {
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        Self::extract_with_relations(file_path, content, repo_id, parser).0
    }

    /// Same nodes as `extract`, plus native import dependencies (Item 2:
    /// `import x`, `from x import y`, relative `from .x import y`) and native
    /// event producer/consumer detection (Item 3: `confluent_kafka`,
    /// `aiokafka`, Celery `@task` / `@app.task` / `.delay()` /
    /// `.apply_async()`).
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> (Vec<ContractNode>, PythonRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut imports: Vec<ImportRef> = Vec::new();
        let mut relations = PythonRelations::default();

        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return (nodes, relations),
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            None,
            &mut nodes,
            &mut imports,
            &mut relations.producers,
            &mut relations.consumers,
        );

        relations.dependencies = Self::resolve_import_dependencies(content, &nodes, &imports);
        (nodes, relations)
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        enclosing: Option<usize>,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<ImportRef>,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
    ) {
        let mut child_enclosing = enclosing;

        match node.kind() {
            "import_statement" => {
                Self::collect_import_names(node, source, imports);
            }
            "import_from_statement" => {
                Self::collect_import_from(node, source, imports);
            }
            "class_definition" => {
                let class_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownClass");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("class {class_name}:"));

                let mut kind = NodeKind::ServiceClass;
                if class_name.ends_with("Servicer") {
                    kind = NodeKind::GrpcService;
                }

                let idx = nodes.len();
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
                child_enclosing = Some(idx);
            }
            "function_definition" => {
                let func_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknown_func");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("def {func_name}():"));

                let mut kind = NodeKind::ServiceClass;
                let mut celery_task_name: Option<String> = None;
                // Check previous siblings for decorators
                if let Some(prev) = node.prev_sibling() {
                    if prev.kind() == "decorator" {
                        if let Ok(dec_text) = prev.utf8_text(source) {
                            // Celery task decorators (`@task`, `@app.task`, `@celery.task`)
                            // are checked first: a Celery app is conventionally named `app`,
                            // so `@app.task` would otherwise also match the generic
                            // `@app.`-prefix HTTP-endpoint heuristic below.
                            if Self::is_celery_task_decorator(dec_text) {
                                kind = NodeKind::Queue;
                                celery_task_name = Self::celery_task_name_override(dec_text)
                                    .or_else(|| Some(func_name.to_string()));
                            } else if dec_text.contains("@app.") || dec_text.contains("@router.") {
                                kind = NodeKind::HttpEndpoint;
                            }
                        }
                    }
                }

                let idx = nodes.len();
                if let Some(task_name) = celery_task_name {
                    consumers.push((idx, CompactStr::new(task_name)));
                }

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(func_name),
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
                child_enclosing = Some(idx);
            }
            "call" => {
                if let Some(idx) = enclosing {
                    Self::detect_event_call(node, source, idx, producers, consumers);
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
                child_enclosing,
                nodes,
                imports,
                producers,
                consumers,
            );
        }
    }

    // ---- Item 2: import extraction --------------------------------------

    fn collect_import_names(node: Node, source: &[u8], imports: &mut Vec<ImportRef>) {
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            Self::push_name_ref(name, source, false, imports);
        }
    }

    fn collect_import_from(node: Node, source: &[u8], imports: &mut Vec<ImportRef>) {
        let has_wildcard = node
            .children(&mut node.walk())
            .any(|c| c.kind() == "wildcard_import");

        if let Some(module) = node.child_by_field_name("module_name") {
            match module.kind() {
                // Relative import (`from .x import y` / `from ..x import y` /
                // `from . import y`): the module text itself can't be matched
                // against a body substring reliably, so mark it module-like
                // (always "used") rather than silently dropping it.
                "relative_import" => {
                    if let Ok(text) = module.utf8_text(source) {
                        imports.push(ImportRef {
                            target: text.to_string(),
                            search_text: text.to_string(),
                            module_like: true,
                        });
                    }
                }
                "dotted_name" => {
                    if let Ok(text) = module.utf8_text(source) {
                        imports.push(ImportRef {
                            target: text.to_string(),
                            search_text: text.to_string(),
                            module_like: has_wildcard,
                        });
                    }
                }
                _ => {}
            }
        }

        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            Self::push_name_ref(name, source, false, imports);
        }
    }

    fn push_name_ref(name: Node, source: &[u8], module_like: bool, imports: &mut Vec<ImportRef>) {
        match name.kind() {
            "dotted_name" => {
                if let Ok(text) = name.utf8_text(source) {
                    imports.push(ImportRef {
                        target: text.to_string(),
                        search_text: text.to_string(),
                        module_like,
                    });
                }
            }
            "aliased_import" => {
                let orig = name
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok());
                let alias = name
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok());
                if let Some(orig) = orig {
                    imports.push(ImportRef {
                        target: orig.to_string(),
                        search_text: alias.unwrap_or(orig).to_string(),
                        module_like,
                    });
                }
            }
            _ => {}
        }
    }

    /// Reuses the TypeScript "is it actually used in this node's line range?"
    /// heuristic (`languages/mod.rs` ~185-205): a file-level import only
    /// attaches to the symbols whose body text actually references it,
    /// except for the module-like cases (relative / wildcard imports) that
    /// cannot be matched that way and are attached everywhere instead.
    fn resolve_import_dependencies(
        content: &str,
        nodes: &[ContractNode],
        imports: &[ImportRef],
    ) -> Vec<(usize, CompactStr)> {
        let mut deps = Vec::new();
        if imports.is_empty() || nodes.is_empty() {
            return deps;
        }

        let content_lines: Vec<&str> = content.lines().collect();
        let mut seen: HashSet<(usize, String)> = HashSet::new();

        for (i, node) in nodes.iter().enumerate() {
            let body = content_lines
                .get(node.line_start.saturating_sub(1)..node.line_end)
                .unwrap_or(&[]);

            for imp in imports {
                if imp.target.is_empty() {
                    continue;
                }
                let is_used =
                    imp.module_like || body.iter().any(|l| l.contains(imp.search_text.as_str()));
                if is_used && seen.insert((i, imp.target.clone())) {
                    deps.push((i, CompactStr::new(imp.target.as_str())));
                }
            }
        }

        deps
    }

    // ---- Item 3: native event producer/consumer detection ---------------

    /// Recognises, on a `call` node, the dominant client-library shapes for
    /// `confluent_kafka` / `aiokafka` (`.produce()`, `.send()`,
    /// `.send_and_wait()`, `.publish()` as producers; `.subscribe()` and the
    /// `AIOKafkaConsumer(...)` constructor as consumers) and Celery
    /// (`.delay()` / `.apply_async()` as producers). Topic/task names are
    /// frequently variables rather than literals; the literal case is
    /// unwrapped, and the non-literal case still emits the variable's source
    /// text as the target instead of being dropped.
    fn detect_event_call(
        node: Node,
        source: &[u8],
        enclosing: usize,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };

        match func.kind() {
            "attribute" => {
                let Some(method_node) = func.child_by_field_name("attribute") else {
                    return;
                };
                let Ok(method) = method_node.utf8_text(source) else {
                    return;
                };

                match method {
                    "delay" | "apply_async" => {
                        // Celery producer call: the task name is the call's
                        // receiver (e.g. `process_order.delay(...)` ->
                        // `process_order`), not an argument.
                        if let Some(object) = func.child_by_field_name("object") {
                            if let Ok(object_text) = object.utf8_text(source) {
                                let task_name =
                                    object_text.rsplit('.').next().unwrap_or(object_text);
                                if !task_name.is_empty() {
                                    producers.push((enclosing, CompactStr::new(task_name)));
                                }
                            }
                        }
                    }
                    "produce" | "send" | "send_and_wait" | "publish" => {
                        if let Some(topic) = Self::first_arg_value(args, source, "topic") {
                            producers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                    "subscribe" => {
                        for topic in Self::list_or_scalar_values(args, source) {
                            consumers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                    _ => {}
                }
            }
            "identifier" => {
                if let Ok(name) = func.utf8_text(source) {
                    if name == "AIOKafkaConsumer" {
                        for topic in Self::positional_string_values(args, source) {
                            consumers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn is_celery_task_decorator(dec_text: &str) -> bool {
        dec_text.contains("@task") || dec_text.contains("@celery") || dec_text.contains(".task")
    }

    /// Pulls an explicit `name="..."` override out of a decorator like
    /// `@app.task(name="my.task")`, falling back to the function's own name
    /// (done by the caller) when there is none.
    fn celery_task_name_override(dec_text: &str) -> Option<String> {
        let name_idx = dec_text.find("name")?;
        let rest = &dec_text[name_idx + 4..];
        let eq_idx = rest.find('=')?;
        let after_eq = rest[eq_idx + 1..].trim_start();
        let quote = after_eq.chars().next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        let after_quote = &after_eq[quote.len_utf8()..];
        let end = after_quote.find(quote)?;
        Some(after_quote[..end].to_string())
    }

    /// The first positional argument's value, or the named keyword's value
    /// if there is no positional argument. Returns the unwrapped literal for
    /// a string, or the raw source text otherwise (the non-literal /
    /// variable-name fallback).
    fn first_arg_value(args: Node, source: &[u8], kw_name: &str) -> Option<String> {
        let mut cursor = args.walk();
        let mut kw_val: Option<String> = None;
        for child in args.children(&mut cursor) {
            if !child.is_named() || child.kind() == "comment" {
                continue;
            }
            if child.kind() == "keyword_argument" {
                if kw_val.is_none() {
                    let matches_name = child
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                        == Some(kw_name);
                    if matches_name {
                        if let Some(value) = child.child_by_field_name("value") {
                            kw_val = Some(Self::expr_value(value, source));
                        }
                    }
                }
                continue;
            }
            return Some(Self::expr_value(child, source));
        }
        kw_val
    }

    /// Every positional argument up to (excluding) the first keyword
    /// argument — used for constructors like `AIOKafkaConsumer('topic', ...)`
    /// where topics are leading positional args.
    fn positional_string_values(args: Node, source: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            if !child.is_named() || child.kind() == "comment" {
                continue;
            }
            if child.kind() == "keyword_argument" {
                break;
            }
            out.push(Self::expr_value(child, source));
        }
        out
    }

    /// The argument's values: every element if it is a list literal,
    /// otherwise the single scalar value (literal or variable name).
    fn list_or_scalar_values(args: Node, source: &[u8]) -> Vec<String> {
        let mut cursor = args.walk();
        let Some(first) = args
            .children(&mut cursor)
            .find(|c| c.is_named() && c.kind() != "comment")
        else {
            return Vec::new();
        };

        if first.kind() == "list" || first.kind() == "tuple" {
            let mut inner_cursor = first.walk();
            first
                .children(&mut inner_cursor)
                .filter(|c| c.is_named())
                .map(|c| Self::expr_value(c, source))
                .collect()
        } else {
            vec![Self::expr_value(first, source)]
        }
    }

    fn expr_value(node: Node, source: &[u8]) -> String {
        if node.kind() == "string" {
            if let Some(v) = Self::string_literal_value(node, source) {
                return v;
            }
        }
        node.utf8_text(source).unwrap_or("").trim().to_string()
    }

    /// Unwraps a simple single/double-quoted string literal's contents.
    fn string_literal_value(node: Node, source: &[u8]) -> Option<String> {
        let text = node.utf8_text(source).ok()?.trim();
        let quote_pos = text.find(['\'', '"'])?;
        let quote = text.as_bytes()[quote_pos] as char;
        let after = &text[quote_pos + quote.len_utf8()..];
        let end = after.rfind(quote)?;
        Some(after[..end].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::ContractGraph;

    fn make_parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_python::LANGUAGE.into();
        parser.set_language(&lang).expect("load python grammar");
        parser
    }

    #[test]
    fn test_python_extractor() {
        let code = r#"
class UserServicer:
    def GetUser(self, request, context):
        pass

@app.get("/users")
def list_users():
    return []
"#;
        let mut parser = make_parser();

        let nodes = PythonExtractor::extract(Path::new("services/user.py"), code, 3, &mut parser);
        assert!(nodes
            .iter()
            .any(|n| n.name == "UserServicer" && n.kind == NodeKind::GrpcService));
        assert!(nodes.iter().any(|n| n.name == "GetUser"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "list_users" && n.kind == NodeKind::HttpEndpoint));
    }

    /// Item 2 (plain `from x import y`): a symbol declared in one file and
    /// imported (and used) in another links the two through `find_dependents`.
    #[test]
    fn test_python_plain_import_dependency() {
        let producer_code = r#"
def greet_user(name):
    return f"Hello {name}"
"#;
        let consumer_code = r#"
from other import greet_user

def process():
    return greet_user("bob")
"#;

        let mut graph = ContractGraph::default();
        let mut parser = make_parser();

        let producer_nodes = PythonExtractor::extract(
            Path::new("services/other.py"),
            producer_code,
            1,
            &mut parser,
        );
        for node in producer_nodes {
            graph.add_node(node);
        }

        let (consumer_nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/main.py"),
            consumer_code,
            1,
            &mut parser,
        );
        let ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, target) in relations.dependencies {
            graph.add_dependency(ids[i], target.as_str());
        }

        let dependents = graph.find_dependents("greet_user");
        assert!(
            dependents.iter().any(|n| n.name == "process"),
            "expected `process` (consumer of greet_user) in find_dependents, got: {:?}",
            dependents
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Item 2 (relative `from .x import y`): same as above, through a
    /// relative import.
    #[test]
    fn test_python_relative_import_dependency() {
        let producer_code = r#"
def greet_user(name):
    return f"Hello {name}"
"#;
        let consumer_code = r#"
from .other import greet_user

def process():
    return greet_user("bob")
"#;

        let mut graph = ContractGraph::default();
        let mut parser = make_parser();

        let producer_nodes = PythonExtractor::extract(
            Path::new("services/other.py"),
            producer_code,
            1,
            &mut parser,
        );
        for node in producer_nodes {
            graph.add_node(node);
        }

        let (consumer_nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/main.py"),
            consumer_code,
            1,
            &mut parser,
        );
        let ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, target) in relations.dependencies {
            graph.add_dependency(ids[i], target.as_str());
        }

        let dependents = graph.find_dependents("greet_user");
        assert!(
            dependents.iter().any(|n| n.name == "process"),
            "expected `process` (consumer of greet_user via relative import) in find_dependents, got: {:?}",
            dependents.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
        );
    }

    /// Item 3 (confluent_kafka / aiokafka): a `.produce()` call in one file
    /// and a `.subscribe()` loop in another are linked through
    /// `analyze_impact`, with no custom pattern configured.
    #[test]
    fn test_python_kafka_producer_consumer_impact() {
        let producer_code = r#"
from confluent_kafka import Producer

def send_order_created(order):
    producer = Producer({"bootstrap.servers": "localhost"})
    producer.produce("orders", order)
"#;
        let consumer_code = r#"
from confluent_kafka import Consumer

def run_consumer_loop():
    consumer = Consumer({"bootstrap.servers": "localhost"})
    consumer.subscribe(["orders"])
    while True:
        msg = consumer.poll(1.0)
"#;

        let mut graph = ContractGraph::default();
        let mut parser = make_parser();

        let (producer_nodes, producer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer.py"),
            producer_code,
            1,
            &mut parser,
        );
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in producer_relations.producers {
            graph.add_producer(producer_ids[i], topic.as_str());
        }

        let (consumer_nodes, consumer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/consumer.py"),
            consumer_code,
            1,
            &mut parser,
        );
        let consumer_ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in consumer_relations.consumers {
            graph.add_consumer(consumer_ids[i], topic.as_str());
        }

        let impact = graph.analyze_impact("orders");
        assert!(impact
            .upstream_producers
            .iter()
            .any(|n| n.name == "send_order_created"));
        assert!(impact
            .downstream_consumers
            .iter()
            .any(|n| n.name == "run_consumer_loop"));
    }

    /// Item 3 (Celery): `.delay()` as a producer and `@app.task` as a
    /// consumer are linked through `analyze_impact`.
    #[test]
    fn test_python_celery_producer_consumer_impact() {
        let producer_code = r#"
from myapp.tasks import process_order

def handle_request(order_id):
    process_order.delay(order_id)
"#;
        let consumer_code = r#"
@app.task
def process_order(order_id):
    pass
"#;

        let mut graph = ContractGraph::default();
        let mut parser = make_parser();

        let (producer_nodes, producer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/client.py"),
            producer_code,
            1,
            &mut parser,
        );
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, task) in producer_relations.producers {
            graph.add_producer(producer_ids[i], task.as_str());
        }

        let (consumer_nodes, consumer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/tasks.py"),
            consumer_code,
            1,
            &mut parser,
        );
        let consumer_ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, task) in consumer_relations.consumers {
            graph.add_consumer(consumer_ids[i], task.as_str());
        }

        let impact = graph.analyze_impact("process_order");
        assert!(impact
            .upstream_producers
            .iter()
            .any(|n| n.name == "handle_request"));
        assert!(impact
            .downstream_consumers
            .iter()
            .any(|n| n.name == "process_order"));
    }

    /// Topic names are frequently variables, not literals: the non-literal
    /// case must still surface the variable's name rather than being dropped.
    #[test]
    fn test_python_variable_topic_name_not_dropped() {
        let code = r#"
from confluent_kafka import Producer

TOPIC = "orders"

def send_event():
    producer = Producer({})
    producer.produce(TOPIC, b"data")
"#;
        let mut parser = make_parser();
        let (nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer.py"),
            code,
            1,
            &mut parser,
        );
        let idx = nodes
            .iter()
            .position(|n| n.name == "send_event")
            .expect("send_event node");

        assert!(
            relations
                .producers
                .iter()
                .any(|(i, topic)| *i == idx && topic.as_str() == "TOPIC"),
            "expected the variable name TOPIC to be emitted, got: {:?}",
            relations.producers
        );
    }
}
