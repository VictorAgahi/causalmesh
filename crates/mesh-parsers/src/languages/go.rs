use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

pub struct GoExtractor;

/// Import/producer/consumer relations extracted alongside `nodes`, expressed as
/// `(local index into the returned node vec, target string)` pairs — the exact
/// shape `mesh_parsers::languages::FileIndex`'s `dependencies`/`producers`/`consumers`
/// fields expect (see `crates/mesh-parsers/src/languages/mod.rs`).
///
#[derive(Debug, Default)]
pub struct GoRelations {
    pub dependencies: Vec<(usize, CompactStr)>,
    pub producers: Vec<(usize, CompactStr)>,
    pub consumers: Vec<(usize, CompactStr)>,
    /// `(caller node index, target RPC service name)` — the generic gRPC
    /// client-call detection (item 5, client side): a `pb.NewFooServiceClient(conn)`
    /// call site, attributed to its smallest enclosing declaration. Feeds
    /// `ContractGraph::add_rpc_call` -> `reconcile_edges`'s `CallsRpc` edges,
    /// which is what lets `find_dependents`/`analyze_grpc` resolve a real
    /// cross-service caller instead of only a package-string match.
    pub rpc_calls: Vec<(usize, CompactStr)>,
}

/// A raw `import_spec`, resolved to the identifier a Go source file would use to
/// reference it (explicit alias, derived default package name, or blank).
struct RawImport {
    ref_ident: CompactStr,
    path: CompactStr,
    is_blank: bool,
}

/// A raw Kafka-shaped producer/consumer signal found either from a topic literal
/// in a `ReaderConfig`/`WriterConfig`-shaped struct literal, or from a
/// `ReadMessage`/`WriteMessages`-shaped call with no literal topic in scope.
struct RawEvent {
    line_start: usize,
    line_end: usize,
    topic: CompactStr,
    is_producer: bool,
}

/// A raw gRPC client-call signal: `pb.NewFooServiceClient(conn)`, found at
/// `(line_start, line_end)`, targeting service `FooService`. Resolved to its
/// smallest enclosing declaration the same way a `RawEvent` is — see
/// `resolve_rpc_calls`.
struct RawRpcCall {
    line_start: usize,
    line_end: usize,
    service_name: CompactStr,
}

impl GoExtractor {
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

    /// Same extraction as `extract`, plus import/event relations for items 2 and 3.
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> (Vec<ContractNode>, GoRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut relations = GoRelations::default();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = CompactStr::default();

        // Pass 1: which struct types were passed as the `srv` argument of a
        // `RegisterXServer(registrar, srv)` call, so method declarations on that
        // type can be classified as `GrpcMethod` rather than plain `ServiceClass`.
        let mut grpc_server_types: HashSet<String> = HashSet::new();
        Self::collect_grpc_server_types(root, source_bytes, &mut grpc_server_types, 0);

        let mut string_consts: HashMap<String, CompactStr> = HashMap::new();
        // Pass 2: `var Topic = getTopic()`-style env-var-with-default idioms,
        // resolved to their fallback literal before the main walk needs them
        // (a composite literal like `kafka.Message{Topic: Topic}`, visited
        // below, looks `Topic` up in this same map).
        Self::collect_getenv_default_consts(root, source_bytes, root, &mut string_consts, 0);
        let mut raw_imports: Vec<RawImport> = Vec::new();
        let mut raw_events: Vec<RawEvent> = Vec::new();
        let mut raw_rpc_calls: Vec<RawRpcCall> = Vec::new();

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
            &grpc_server_types,
            &mut string_consts,
            &mut raw_imports,
            &mut raw_events,
            &mut raw_rpc_calls,
            0,
        );

        Self::resolve_dependencies(content, &nodes, &raw_imports, &mut relations.dependencies);
        Self::resolve_events(
            &mut nodes,
            raw_events,
            &file_path,
            repo_id,
            &package_name,
            &mut relations,
        );
        Self::resolve_rpc_calls(&nodes, raw_rpc_calls, &mut relations);

        (nodes, relations)
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
        grpc_server_types: &HashSet<String>,
        string_consts: &mut HashMap<String, CompactStr>,
        raw_imports: &mut Vec<RawImport>,
        raw_events: &mut Vec<RawEvent>,
        raw_rpc_calls: &mut Vec<RawRpcCall>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        match node.kind() {
            "var_spec" | "const_spec" => {
                Self::collect_single_const_spec(node, source, string_consts);
            }
            "package_clause" => {
                if let Ok(text) = node.utf8_text(source) {
                    let clean = text.trim_start_matches("package ").trim();
                    *package_name = mesh_core::detect_service_package(file_path, Some(clean));
                }
            }
            "import_declaration" => {
                Self::collect_imports(node, source, raw_imports);
            }
            "type_declaration" => {
                if let Ok(text) = node.utf8_text(source) {
                    let first_line = text.lines().next().unwrap_or("").trim();
                    let parts: Vec<&str> = first_line.split_whitespace().collect();
                    if parts.len() >= 2 && parts[0] == "type" {
                        let type_name = parts[1];
                        let kind = if first_line.contains("interface") {
                            NodeKind::Interface
                        } else {
                            NodeKind::ServiceClass
                        };

                        nodes.push(ContractNode {
                            id: 0,
                            name: CompactStr::new(type_name),
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
            }
            "function_declaration" | "method_declaration" => {
                let func_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownFunc");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| func_name.to_string());

                let receiver_type = if node.kind() == "method_declaration" {
                    Self::method_receiver_type(node, source)
                } else {
                    None
                };

                let is_http_signature = first_line.contains("ResponseWriter")
                    || first_line.contains("Request")
                    || first_line.contains("Context")
                    || first_line.contains("Ctx");

                let mut kind = NodeKind::ServiceClass;
                if receiver_type
                    .as_deref()
                    .is_some_and(|rt| grpc_server_types.contains(rt))
                {
                    kind = NodeKind::GrpcMethod;
                } else if (func_name.starts_with("Handle") || func_name.ends_with("Handler"))
                    && is_http_signature
                {
                    kind = NodeKind::HttpEndpoint;
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
            }
            "call_expression" => {
                Self::handle_call_expression(
                    node,
                    source,
                    file_path,
                    repo_id,
                    package_name,
                    nodes,
                    raw_events,
                    raw_rpc_calls,
                );
            }
            "composite_literal" => {
                Self::handle_composite_literal(node, source, string_consts, raw_events);
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
                grpc_server_types,
                string_consts,
                raw_imports,
                raw_events,
                raw_rpc_calls,
                depth + 1,
            );
        }
    }
    // Import extraction and resolution

    fn collect_imports(decl: Node, source: &[u8], out: &mut Vec<RawImport>) {
        let mut cursor = decl.walk();
        for child in decl.children(&mut cursor) {
            match child.kind() {
                "import_spec" => Self::collect_import_spec(child, source, out),
                "import_spec_list" => {
                    let mut inner = child.walk();
                    for spec in child.children(&mut inner) {
                        if spec.kind() == "import_spec" {
                            Self::collect_import_spec(spec, source, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_import_spec(spec: Node, source: &[u8], out: &mut Vec<RawImport>) {
        let Some(path_node) = spec.child_by_field_name("path") else {
            return;
        };
        let Ok(raw_path_text) = path_node.utf8_text(source) else {
            return;
        };
        let path = raw_path_text.trim_matches('"');
        if path.is_empty() {
            return;
        }

        let (ref_ident, is_blank) = match spec.child_by_field_name("name") {
            Some(name_node) => match name_node.utf8_text(source) {
                Ok("_") => (CompactStr::default(), true),
                Ok(text) => (CompactStr::new(text), false),
                Err(_) => (CompactStr::default(), false),
            },
            None => {
                let last_segment = path.rsplit('/').next().unwrap_or(path);
                let default_ident = last_segment.split('.').next().unwrap_or(last_segment);
                (CompactStr::new(default_ident), false)
            }
        };

        out.push(RawImport {
            ref_ident,
            path: CompactStr::new(path),
            is_blank,
        });
    }

    /// Reuses the TypeScript extractor's "is it actually used in this node's line
    /// range?" heuristic (`languages/mod.rs`, TypeScript branch) so a file-level
    /// import does not attach to every symbol in the file. Blank imports (`_`)
    /// have no referencing identifier to search for — they run only for their
    /// side effects (`init()`), so they are attached to every node in the file,
    /// which is the correct semantics for that case rather than an escape hatch.
    fn resolve_dependencies(
        content: &str,
        nodes: &[ContractNode],
        raw_imports: &[RawImport],
        out: &mut Vec<(usize, CompactStr)>,
    ) {
        if raw_imports.is_empty() || nodes.is_empty() {
            return;
        }
        let content_lines: Vec<&str> = content.lines().collect();

        for imp in raw_imports {
            if imp.is_blank {
                for i in 0..nodes.len() {
                    out.push((i, imp.path.clone()));
                }
                continue;
            }
            if imp.ref_ident.is_empty() {
                continue;
            }
            for (i, n) in nodes.iter().enumerate() {
                let start = n.line_start.saturating_sub(1);
                let end = n.line_end;
                let body = content_lines.get(start..end).unwrap_or(&[]);
                if body.iter().any(|l| l.contains(imp.ref_ident.as_str())) {
                    out.push((i, imp.path.clone()));
                }
            }
        }
    }
    // Kafka / event producer and consumer detection

    #[allow(clippy::too_many_arguments)]
    fn handle_call_expression(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
        raw_events: &mut Vec<RawEvent>,
        raw_rpc_calls: &mut Vec<RawRpcCall>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "selector_expression" {
            return;
        }
        let Some(field) = func.child_by_field_name("field") else {
            return;
        };
        let Ok(method) = field.utf8_text(source) else {
            return;
        };

        // gRPC client construction: `pb.NewFooServiceClient(conn)` — the
        // standard `protoc-gen-go-grpc` client constructor, symmetric with
        // the `RegisterFooServiceServer` server-side detection right below.
        // Recorded here (raw, line-range only) and attributed to its smallest
        // enclosing declaration in `resolve_rpc_calls`, the same two-pass
        // shape already used for Kafka producer/consumer detection above.
        let operand_name = func
            .child_by_field_name("operand")
            .and_then(|o| o.utf8_text(source).ok())
            .unwrap_or("");
        let is_third_party_client = matches!(
            operand_name,
            "redis"
                | "http"
                | "mongo"
                | "s3"
                | "sqs"
                | "vault"
                | "elastic"
                | "sql"
                | "db"
                | "kafka"
                | "sarama"
        );

        if !is_third_party_client && method.starts_with("New") && method.ends_with("Client") {
            let service_name = &method["New".len()..method.len() - "Client".len()];
            if !service_name.is_empty() {
                raw_rpc_calls.push(RawRpcCall {
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    service_name: CompactStr::new(service_name),
                });
            }
        }

        // gRPC server registration: `pb.RegisterFooServiceServer(grpcServer, srv)`.
        if method.starts_with("Register") && method.ends_with("Server") {
            let service_name = &method["Register".len()..method.len() - "Server".len()];
            if !service_name.is_empty() {
                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_default();
                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(service_name),
                    kind: NodeKind::GrpcService,
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

        // chi/gin/echo route registration: `r.Get("/health", handler)`,
        // `router.GET("/users/:id", getUser)`.
        let lower_method = method.to_ascii_lowercase();
        if matches!(
            lower_method.as_str(),
            "get" | "post" | "put" | "delete" | "patch" | "head" | "options"
        ) {
            if let Some(args) = node.child_by_field_name("arguments") {
                if args.named_child_count() >= 2 {
                    if let Some(first_arg) = args.named_child(0) {
                        if first_arg.kind() == "interpreted_string_literal" {
                            if let Ok(txt) = first_arg.utf8_text(source) {
                                let route_path = txt.trim_matches('"');
                                if route_path.starts_with('/') {
                                    let ep_name =
                                        format!("{} {route_path}", method.to_ascii_uppercase());
                                    nodes.push(ContractNode {
                                        id: 0,
                                        name: CompactStr::new(ep_name.as_str()),
                                        kind: NodeKind::HttpEndpoint,
                                        file_path: file_path.clone(),
                                        line_start: node.start_position().row + 1,
                                        line_end: node.end_position().row + 1,
                                        package: package_name.clone(),
                                        repo_id,
                                        signature: Some(CompactStr::new(ep_name.as_str())),
                                        docstring: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // kafka-go / sarama / confluent-kafka-go producer/consumer calls. Only a
        // genuine string-literal topic argument is recorded (e.g.
        // `ConsumePartition("orders", ...)`, `SubscribeTopics([]string{"orders"})`).
        // A call with no literal argument (the topic is held in a variable) records
        // nothing — the receiver's own identifier used to be emitted as a stand-in
        // "topic" (e.g. `reader.ReadMessage(...)` recording the topic "reader"),
        // which is not a topic name at all, just the name of the variable calling
        // the method.
        let is_producer_call = matches!(method, "WriteMessages" | "SendMessage" | "Produce");
        let is_consumer_call = matches!(
            method,
            "ReadMessage" | "FetchMessage" | "ConsumePartition" | "SubscribeTopics" | "Subscribe"
        );
        if is_producer_call || is_consumer_call {
            let mut literals = Vec::new();
            if let Some(args) = node.child_by_field_name("arguments") {
                Self::collect_string_literals(args, source, &mut literals, 0);
            }
            let line_start = node.start_position().row + 1;
            let line_end = node.end_position().row + 1;
            for lit in literals {
                raw_events.push(RawEvent {
                    line_start,
                    line_end,
                    topic: CompactStr::new(lit.as_str()),
                    is_producer: is_producer_call,
                });
            }
        }
    }

    /// Topic literals in `ReaderConfig`/`WriterConfig`-shaped struct literals, e.g.
    /// `kafka.ReaderConfig{Topic: "orders"}`, `kafka.Message{Topic: "orders"}`,
    /// `sarama.ProducerMessage{Topic: "orders"}`.
    fn handle_composite_literal(
        node: Node,
        source: &[u8],
        string_consts: &HashMap<String, CompactStr>,
        raw_events: &mut Vec<RawEvent>,
    ) {
        let Some(type_name) = Self::composite_type_name(node, source) else {
            return;
        };
        let is_producer = match type_name.as_str() {
            "ReaderConfig" => false,
            "WriterConfig" | "ProducerMessage" | "Message" | "Writer" => true,
            _ => return,
        };
        let Some(topic) = Self::extract_topic_field(node, source, string_consts) else {
            return;
        };
        raw_events.push(RawEvent {
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            topic,
            is_producer,
        });
    }

    fn composite_type_name(node: Node, source: &[u8]) -> Option<String> {
        let type_node = node.child_by_field_name("type")?;
        match type_node.kind() {
            "type_identifier" => type_node.utf8_text(source).ok().map(str::to_string),
            "qualified_type" => type_node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string),
            _ => None,
        }
    }

    fn extract_topic_field(
        literal: Node,
        source: &[u8],
        string_consts: &HashMap<String, CompactStr>,
    ) -> Option<CompactStr> {
        let body = literal.child_by_field_name("body")?;
        let mut cursor = body.walk();
        for elem in body.children(&mut cursor) {
            if elem.kind() != "keyed_element" {
                continue;
            }
            let Some(key) = elem.child_by_field_name("key") else {
                continue;
            };
            let Ok(key_text) = key.utf8_text(source) else {
                continue;
            };
            if key_text == "Topic" {
                let value = elem.child_by_field_name("value")?;
                return Self::extract_topic_value_text(value, source, string_consts);
            }
            if key_text == "TopicPartition" {
                if let Some(val) = elem.child_by_field_name("value") {
                    let mut inner = val;
                    if inner.kind() == "literal_element" {
                        inner = inner.named_child(0).unwrap_or(inner);
                    }
                    if inner.kind() == "unary_expression" {
                        inner = inner.child_by_field_name("operand").unwrap_or(inner);
                    }
                    if inner.kind() == "composite_literal" {
                        if let Some(topic) = Self::extract_topic_field(inner, source, string_consts)
                        {
                            return Some(topic);
                        }
                    }
                }
            }
        }
        None
    }

    fn extract_topic_value_text(
        value: Node,
        source: &[u8],
        string_consts: &HashMap<String, CompactStr>,
    ) -> Option<CompactStr> {
        let mut literals = Vec::new();
        Self::collect_string_literals(value, source, &mut literals, 0);
        if let Some(first) = literals.into_iter().next() {
            return Some(CompactStr::new(first.as_str()));
        }
        let mut unwrapped = if value.kind() == "literal_element" {
            value.named_child(0).unwrap_or(value)
        } else {
            value
        };
        if unwrapped.kind() == "unary_expression" {
            unwrapped = unwrapped
                .child_by_field_name("operand")
                .unwrap_or(unwrapped);
        }
        let bare_name = match unwrapped.kind() {
            "identifier" => unwrapped.utf8_text(source).ok(),
            "selector_expression" => unwrapped
                .child_by_field_name("field")
                .and_then(|f| f.utf8_text(source).ok()),
            _ => None,
        };
        if let Some(resolved) = bare_name.and_then(|name| string_consts.get(name)) {
            return Some(resolved.clone());
        }
        // A constant declared in another file/package (`kafka.Topic`, resolved
        // only against this *file's* `string_consts`), a runtime config
        // lookup, or an unresolved `&topicVar`: none of these have a value
        // available here. This used to fall back to the raw expression text
        // itself (`"kafka.Topic"`, the qualified reference, not a topic name)
        // — exactly the class of fabricated value P0 step 1.7 eliminated
        // everywhere else; recording nothing is correct here too.
        None
    }

    /// Collects every plain string literal under `node` — but never descends
    /// into a *named* struct literal (`kafka.Message{...}`,
    /// `sarama.ProducerMessage{...}`), only into anonymous ones like
    /// `[]string{"orders"}`. A call like `producer.Produce(&kafka.Message{
    /// Value: []byte("payload") })` used to have this walk straight through
    /// the whole argument tree and pick up `"payload"` — a completely
    /// unrelated field's literal — as if it were the topic, because nothing
    /// stopped the recursion at the struct literal's boundary. Named struct
    /// literals have their own dedicated, field-aware extraction
    /// (`handle_composite_literal`/`extract_topic_field`); this generic walk
    /// is only for simple positional/slice-literal call arguments.
    fn collect_string_literals(node: Node, source: &[u8], out: &mut Vec<String>, depth: usize) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "interpreted_string_literal" {
            if let Ok(text) = node.utf8_text(source) {
                let unquoted = text.trim_matches('"');
                if !unquoted.is_empty() {
                    out.push(unquoted.to_string());
                }
            }
            return;
        }
        if node.kind() == "composite_literal" {
            let is_named_struct = node
                .child_by_field_name("type")
                .is_some_and(|t| matches!(t.kind(), "qualified_type" | "type_identifier"));
            if is_named_struct {
                return;
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_string_literals(child, source, out, depth + 1);
        }
    }

    /// Attaches each raw producer/consumer signal to the declaration node whose
    /// line range encloses it (the smallest enclosing range wins), so the edge is
    /// anchored to the function/method that actually does the work. Falls back
    /// to a synthetic marker node — mirroring the existing convention for
    /// AsyncAPI channels and custom patterns (`EventStream/PostProcessor`) and
    /// the Java extractor's `KafkaTopic` consumer markers — when no declaration
    /// encloses it (e.g. a package-level `var` initializer).
    fn resolve_events(
        nodes: &mut Vec<ContractNode>,
        raw_events: Vec<RawEvent>,
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        relations: &mut GoRelations,
    ) {
        for event in raw_events {
            let enclosing = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.line_start <= event.line_start && n.line_end >= event.line_end)
                .min_by_key(|(_, n)| n.line_end - n.line_start);

            let idx = match enclosing {
                Some((i, _)) => i,
                None => {
                    let kind = if event.is_producer {
                        NodeKind::EventStream
                    } else {
                        NodeKind::KafkaTopic
                    };
                    let signature = if event.is_producer {
                        format!("Producer of {}", event.topic)
                    } else {
                        format!("Consumer of {}", event.topic)
                    };
                    nodes.push(ContractNode {
                        id: 0,
                        name: event.topic.clone(),
                        kind,
                        file_path: file_path.clone(),
                        line_start: event.line_start,
                        line_end: event.line_end,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(signature.as_str())),
                        docstring: None,
                    });
                    nodes.len() - 1
                }
            };

            if event.is_producer {
                relations.producers.push((idx, event.topic));
            } else {
                relations.consumers.push((idx, event.topic));
            }
        }
    }

    /// Attaches each raw `New<Service>Client(...)` call to its smallest
    /// enclosing declaration, mirroring `resolve_events` above — but unlike a
    /// producer/consumer signal, a call site with no enclosing declaration
    /// (e.g. a package-level `var` initializer) has no sensible caller to
    /// attribute a `CallsRpc` edge to, so it is simply dropped rather than
    /// given a synthetic node.
    fn resolve_rpc_calls(
        nodes: &[ContractNode],
        raw_rpc_calls: Vec<RawRpcCall>,
        relations: &mut GoRelations,
    ) {
        for call in raw_rpc_calls {
            let enclosing = nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.line_start <= call.line_start && n.line_end >= call.line_end)
                .min_by_key(|(_, n)| n.line_end - n.line_start);

            if let Some((idx, _)) = enclosing {
                relations.rpc_calls.push((idx, call.service_name));
            }
        }
    }

    /// Pre-pass collecting `var X = "literal"` / `const X = "literal"`
    /// string assignments (any scope, keyed on the bare identifier — good
    /// enough to resolve both a bare reference and the common
    /// `pkg.X`-via-selector-expression reference shape, since callers only
    /// ever look up the bare field name) into a lookup, so a composite
    /// literal field value that references `X` can be resolved to the real
    /// string instead of falling back to raw expression text. Deliberately
    /// bounded: does NOT resolve through a function call's return value
    /// (e.g. `var Topic = getTopic()`, whose body computes the string
    /// conditionally via env-var lookup) — that requires real control-flow
    /// interpretation, out of scope here; such a reference simply won't be
    /// found in this map and falls back to raw text as before.
    fn collect_single_const_spec(node: Node, source: &[u8], out: &mut HashMap<String, CompactStr>) {
        let Some(value_list) = node.child_by_field_name("value") else {
            return;
        };
        let mut names = Vec::new();
        let mut cursor = node.walk();
        for child in node.children_by_field_name("name", &mut cursor) {
            if let Ok(name) = child.utf8_text(source) {
                names.push(name);
            }
        }
        let values: Vec<Node> = value_list.named_children(&mut value_list.walk()).collect();
        for (name, value_node) in names.into_iter().zip(values) {
            if matches!(
                value_node.kind(),
                "interpreted_string_literal" | "raw_string_literal"
            ) {
                if let Ok(value) = value_node.utf8_text(source) {
                    let unquoted = value.trim_matches(|c: char| c == '"' || c == '`');
                    if !unquoted.is_empty() {
                        out.insert(name.to_string(), CompactStr::new(unquoted));
                    }
                }
            }
        }
    }
    // gRPC server registration and method extraction

    fn collect_grpc_server_types(
        node: Node,
        source: &[u8],
        out: &mut HashSet<String>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "selector_expression" {
                    if let Some(field) = func.child_by_field_name("field") {
                        if let Ok(method) = field.utf8_text(source) {
                            if method.starts_with("Register") && method.ends_with("Server") {
                                if let Some(args) = node.child_by_field_name("arguments") {
                                    if let Some(second) = args.named_child(1) {
                                        if let Some(type_name) =
                                            Self::extract_receiver_type_name(second, source)
                                        {
                                            out.insert(type_name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_grpc_server_types(child, source, out, depth + 1);
        }
    }

    /// A real, common Go idiom this repo's own extraction previously refused
    /// to resolve at all: `var Topic = getTopic()`, where `getTopic` reads an
    /// env var and falls back to a literal default —
    /// `if v := os.Getenv("KAFKA_TOPIC"); v != "" { return v }; return "orders"`
    /// (verbatim from the real OpenTelemetry demo's `checkout/kafka/producer.go`).
    /// Resolves `Topic` to that fallback literal — the only statically-known
    /// value; the real runtime value may come from the environment instead,
    /// but the literal fallback is what a deployment overwhelmingly runs
    /// with, and it's the best static signal available without actually
    /// running the program. A function whose body has no `os.Getenv`/
    /// `os.LookupEnv` call at all is left alone entirely: this is
    /// deliberately narrow (an env-var-with-default idiom specifically),
    /// not "resolve any function that happens to return a string literal
    /// somewhere" — the general case is a P0-step-1.7-style invented-value
    /// risk, this specific shape is not.
    fn collect_getenv_default_consts(
        node: Node,
        source: &[u8],
        root: Node,
        out: &mut HashMap<String, CompactStr>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "var_spec" || node.kind() == "const_spec" {
            Self::resolve_var_spec_getenv_default(node, root, source, out);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_getenv_default_consts(child, source, root, out, depth + 1);
        }
    }

    fn resolve_var_spec_getenv_default(
        spec: Node,
        root: Node,
        source: &[u8],
        out: &mut HashMap<String, CompactStr>,
    ) {
        let Some(value_list) = spec.child_by_field_name("value") else {
            return;
        };
        let mut name_cursor = spec.walk();
        let names: Vec<&str> = spec
            .children_by_field_name("name", &mut name_cursor)
            .filter_map(|n| n.utf8_text(source).ok())
            .collect();
        let values: Vec<Node> = value_list.named_children(&mut value_list.walk()).collect();

        for (name, value_node) in names.into_iter().zip(values) {
            if out.contains_key(name) {
                continue; // a genuine literal already resolved this name
            }
            if value_node.kind() != "call_expression" {
                continue;
            }
            let Some(func) = value_node.child_by_field_name("function") else {
                continue;
            };
            if func.kind() != "identifier" {
                continue; // only a bare zero-arg local function call, e.g. `getTopic()`
            }
            let Some(args) = value_node.child_by_field_name("arguments") else {
                continue;
            };
            if args.named_child_count() != 0 {
                continue;
            }
            let Ok(func_name) = func.utf8_text(source) else {
                continue;
            };
            if let Some(func_node) = Self::find_function_by_name(root, source, func_name) {
                if let Some(default) = Self::extract_getenv_fallback_literal(func_node, source) {
                    out.insert(name.to_string(), CompactStr::new(default));
                }
            }
        }
    }

    #[allow(clippy::manual_find)]
    fn find_function_by_name<'a>(root: Node<'a>, source: &[u8], name: &str) -> Option<Node<'a>> {
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            let is_match = child.kind() == "function_declaration"
                && child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(name);
            if is_match {
                return Some(child);
            }
        }
        None
    }

    /// `func_node`'s body must contain a genuine `os.Getenv`/`os.LookupEnv`
    /// call (the whole point of this idiom) before its last `return
    /// "literal"` is trusted as the env-var's fallback default — otherwise
    /// this would just be "the last string literal returned by any
    /// function," an unrelated and much weaker signal.
    /// The function's own final statement, in its own body block, must be an
    /// unconditional `return "literal"` — not merely the *last occurring*
    /// `return "..."` text found anywhere in the function's source (the
    /// first version of this check did that, and a ruthless review found it
    /// could be fooled by an earlier conditional branch returning an
    /// unrelated literal before falling through to a real, dynamically
    /// computed default; by a `return "..."` sitting inside a `//` comment;
    /// or by one inside a nested closure/goroutine literal). Requiring the
    /// literal to be the function's own last top-level AST statement means
    /// none of those can be mistaken for the real, unconditionally-reached
    /// fallback — a conditional branch's return is never the *last*
    /// statement, a comment isn't in the AST at all, and a nested closure's
    /// body is a separate node this function's own `named_child` list never
    /// descends into.
    fn extract_getenv_fallback_literal(func_node: Node, source: &[u8]) -> Option<String> {
        let text = func_node.utf8_text(source).ok()?;
        if !text.contains("os.Getenv(") && !text.contains("os.LookupEnv(") {
            return None;
        }
        let body = func_node.child_by_field_name("body")?;
        // tree-sitter-go's grammar keeps `comment` as a genuine *named*
        // sibling statement inside a block, so the literal last named child
        // can be a trailing `// comment` rather than the real last
        // statement — skip over any.
        let mut cursor = body.walk();
        let last_stmt = body
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .last()?;
        if last_stmt.kind() != "return_statement" {
            return None;
        }
        // `return "x"`'s value sits inside the statement's own
        // `expression_list`, not directly as the statement's child.
        let expr_list = last_stmt.named_child(0)?;
        let expr = if expr_list.kind() == "expression_list" {
            expr_list.named_child(0)?
        } else {
            expr_list
        };
        if !matches!(
            expr.kind(),
            "interpreted_string_literal" | "raw_string_literal"
        ) {
            return None;
        }
        let raw = expr.utf8_text(source).ok()?;
        let unquoted = raw.trim_matches(|c: char| c == '"' || c == '`');
        (!unquoted.is_empty()).then(|| unquoted.to_string())
    }

    fn extract_receiver_type_name(node: Node, source: &[u8]) -> Option<String> {
        match node.kind() {
            "unary_expression" => node
                .child_by_field_name("operand")
                .and_then(|n| Self::extract_receiver_type_name(n, source)),
            "composite_literal" => Self::composite_type_name(node, source),
            _ => None,
        }
    }

    fn method_receiver_type(node: Node, source: &[u8]) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let param = receiver.named_child(0)?;
        let type_node = param.child_by_field_name("type")?;
        match type_node.kind() {
            "pointer_type" => type_node
                .named_child(0)
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string),
            _ => type_node.utf8_text(source).ok().map(str::to_string),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::ContractGraph;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_go::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_go_extractor() {
        let code = r#"
package auth

type Server struct {}

func (s *Server) AuthenticateUser(ctx context.Context, req *AuthRequest) (*AuthResponse, error) {
    return nil, nil
}
"#;
        let mut parser = parser();
        let nodes = GoExtractor::extract(Path::new("server.go"), code, 2, &mut parser);
        assert!(nodes.iter().any(|n| n.name == "Server"));
        assert!(nodes.iter().any(|n| n.name == "AuthenticateUser"));
    }

    /// A kafka-go/sarama-style call with no string-literal topic argument
    /// (the topic is held in a variable) must not record any topic at all.
    /// It used to fall back to the receiver's own identifier (`reader` on
    /// `reader.ReadMessage(ctx)`), fabricating a "topic" that was really just
    /// the name of the local variable calling the method.
    #[test]
    fn kafka_call_without_literal_topic_records_nothing() {
        let code = r#"
package consumer

func run(reader *kafka.Reader) {
    msg, _ := reader.ReadMessage(ctx)
    _ = msg
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 0, &tree);
        assert!(
            relations.consumers.is_empty(),
            "expected no fabricated topic from the receiver's own name, got: {:?}",
            relations.consumers
        );
    }

    /// A genuine string-literal topic argument is still extracted correctly.
    #[test]
    fn kafka_call_with_literal_topic_is_extracted() {
        let code = r#"
package consumer

func run(reader *kafka.Reader) {
    reader.SubscribeTopics([]string{"orders"})
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 0, &tree);
        assert_eq!(relations.consumers.len(), 1);
        assert_eq!(relations.consumers[0].1.as_str(), "orders");
    }
    // Import dependency wiring tests

    #[test]
    fn find_dependents_links_consumer_via_go_import() {
        // Producer file: declares the package that will be imported. Its own
        // declarations are irrelevant to `add_dependency`/`find_dependents` —
        // only the import *path string* used by the consumer matters, mirroring
        // `ContractGraph::add_dependency`'s "dynamic link resolved at query time"
        // contract (mesh-core/src/contracts.rs).
        let producer_code = r#"
package utils

func Helper() string {
    return "ok"
}
"#;
        let mut p1 = parser();
        let producer_nodes =
            GoExtractor::extract(Path::new("utils/helper.go"), producer_code, 1, &mut p1);
        assert!(producer_nodes.iter().any(|n| n.name == "Helper"));

        // Consumer file: imports the producer's package and uses it.
        let consumer_code = r#"
package main

import (
    "myapp/utils"
)

func Run() {
    utils.Helper()
}
"#;
        let mut p2 = parser();
        let consumer_tree = p2.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, relations) = GoExtractor::extract_with_relations(
            Path::new("main.go"),
            consumer_code,
            1,
            &consumer_tree,
        );
        assert!(consumer_nodes.iter().any(|n| n.name == "Run"));
        assert!(relations
            .dependencies
            .iter()
            .any(|(_, target)| target.as_str() == "myapp/utils"));

        // Wire it into a graph exactly as `FileIndex::apply` would (mod.rs),
        // then confirm `find_dependents` resolves the consumer.
        let mut graph = ContractGraph::new();
        let ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, target) in relations.dependencies {
            graph.add_dependency(ids[i], target.as_str());
        }

        let dependents = graph.find_dependents("myapp/utils");
        assert!(
            dependents.iter().any(|n| n.name == "Run"),
            "expected `Run` to depend on myapp/utils, got: {dependents:?}"
        );
    }

    #[test]
    fn blank_import_is_not_silently_dropped() {
        let code = r#"
package main

import (
    _ "github.com/lib/pq"
)

func Init() {}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("main.go"), code, 1, &tree);
        assert!(relations
            .dependencies
            .iter()
            .any(|(_, target)| target.as_str() == "github.com/lib/pq"));
    }
    // Kafka producer / consumer linking tests

    #[test]
    fn analyze_impact_links_kafka_go_producer_and_consumer() {
        let producer_code = r#"
package producer

func Emit() {
    writer := kafka.NewWriter(kafka.WriterConfig{
        Brokers: []string{"localhost:9092"},
        Topic:   "orders",
    })
    writer.WriteMessages(ctx, kafka.Message{Topic: "orders"})
}
"#;
        let consumer_code = r#"
package consumer

func Consume() {
    reader := kafka.NewReader(kafka.ReaderConfig{
        Brokers: []string{"localhost:9092"},
        Topic:   "orders",
    })
    reader.ReadMessage(ctx)
}
"#;
        let mut p1 = parser();
        let producer_tree = p1.parse(producer_code, None).expect("parse");
        let (producer_nodes, producer_relations) = GoExtractor::extract_with_relations(
            Path::new("producer.go"),
            producer_code,
            1,
            &producer_tree,
        );
        assert!(!producer_relations.producers.is_empty());

        let mut p2 = parser();
        let consumer_tree = p2.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, consumer_relations) = GoExtractor::extract_with_relations(
            Path::new("consumer.go"),
            consumer_code,
            1,
            &consumer_tree,
        );
        assert!(!consumer_relations.consumers.is_empty());

        // No custom regex pattern configured anywhere — purely native detection.
        let mut graph = ContractGraph::new();
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in producer_relations.producers {
            graph.add_producer(producer_ids[i], topic.as_str());
        }
        let consumer_ids: Vec<_> = consumer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in consumer_relations.consumers {
            graph.add_consumer(consumer_ids[i], topic.as_str());
        }

        let impact = graph.analyze_impact("orders");
        assert!(
            !impact.upstream_producers.is_empty(),
            "expected an upstream producer for topic 'orders'"
        );
        assert!(
            !impact.downstream_consumers.is_empty(),
            "expected a downstream consumer for topic 'orders'"
        );
    }

    #[test]
    fn sarama_producer_message_topic_is_detected() {
        let code = r#"
package producer

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: "payments",
        Value: sarama.StringEncoder("hi"),
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(relations
            .producers
            .iter()
            .any(|(_, topic)| topic.as_str() == "payments"));
    }

    /// A `sarama.ProducerMessage{Topic: kafka.Topic}` literal referencing a
    /// package-level `var Topic = "orders"` constant resolves to the
    /// real topic name, rather than raw expression text.
    #[test]
    fn sarama_topic_referencing_a_string_const_resolves_to_its_value() {
        let code = r#"
package kafka

var Topic = "orders"

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: kafka.Topic,
        Value: sarama.StringEncoder("hi"),
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "orders"),
            "expected the resolved topic 'orders', got: {:?}",
            relations.producers
        );
    }

    #[test]
    fn go_raw_string_literal_backtick_resolves_topic_value() {
        let code = r#"
package kafka

const Topic = `orders-v2`

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: Topic,
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "orders-v2"),
            "expected backtick raw string 'orders-v2', got: {:?}",
            relations.producers
        );
    }

    /// `Topic: &topic` — an unresolved local variable, not a `var Topic =
    /// "literal"` this extractor can look up — must record no topic at all.
    /// This used to fall back to the identifier's own name ("topic"), which
    /// is the variable's name, not its value (the same class of fabrication
    /// P0 step 1.7 eliminated in every other producer/consumer call shape).
    #[test]
    fn confluent_kafka_topic_partition_with_unresolved_variable_records_nothing() {
        let code = r#"
package kafka

func Emit() {
    producer.Produce(&kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: 1},
        Value: []byte("payload"),
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations.producers.is_empty(),
            "expected no fabricated topic from an unresolved variable, got: {:?}",
            relations.producers
        );
    }

    /// A topic resolved through a function call's return value (not a direct
    /// `var X = "literal"`), or a constant declared in another file/package
    /// (`kafka.Topic`, resolved only against *this file's* `var`/`const`
    /// declarations) has no value available here — real control-flow
    /// interpretation or cross-file resolution would be needed, neither of
    /// which this extractor does. It must record no topic at all rather than
    /// the raw expression text (`"kafka.Topic"`) it used to fall back to —
    /// caught empirically indexing the OpenTelemetry demo, where
    /// `checkout/main.go`'s `Topic: kafka.Topic` (a cross-package reference)
    /// produced exactly this fabricated node.
    #[test]
    fn sarama_topic_referencing_a_getenv_default_resolves_to_the_fallback_literal() {
        // Verbatim (renamed identifiers aside) from the real OpenTelemetry
        // demo's checkout/kafka/producer.go. P0 step 1.7 correctly refused to
        // fabricate a value here (this exact shape used to record the raw
        // expression text `"kafka.Topic"` as the "topic" — the invented-value
        // bug that step fixed); P1 step 2.5 now resolves it properly instead
        // of leaving the signal dropped, recognizing the specific
        // env-var-with-literal-fallback idiom rather than "any function that
        // happens to return a string."
        let code = r#"
package kafka

var Topic = getTopic()

func getTopic() string {
    if envTopic := os.Getenv("KAFKA_TOPIC"); envTopic != "" {
        return envTopic
    }
    return "orders"
}

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: kafka.Topic,
        Value: sarama.StringEncoder("hi"),
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "orders"),
            "expected the getenv fallback literal 'orders' to be resolved, got: {:?}",
            relations.producers
        );
    }

    /// A function with no `os.Getenv`/`os.LookupEnv` call at all must not
    /// have its trailing return value treated as an env-var fallback — that
    /// would just be "the last string literal any function returns," an
    /// unrelated and much weaker signal than the specific idiom this feature
    /// targets, and exactly the kind of fabrication P0 step 1.7 eliminated.
    #[test]
    fn function_call_without_getenv_is_not_resolved_as_a_fallback() {
        let code = r#"
package kafka

var Topic = computeTopic()

func computeTopic() string {
    return "orders"
}

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: kafka.Topic,
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations.producers.is_empty(),
            "a function with no getenv call must not be treated as an env-var default, got: {:?}",
            relations.producers
        );
    }

    /// The unconditional fallback must be the function's own LAST statement
    /// — not merely the last `return "literal"` text found anywhere in its
    /// source. A ruthless review caught the first (text-scanning) version of
    /// this feature resolving to an unrelated *conditional* branch's literal
    /// when the function's real, unconditional fallback was a dynamically
    /// computed value — worse than P0 step 1.7's "leave it unresolved"
    /// baseline, since it silently produces a *wrong* topic instead of none.
    #[test]
    fn getenv_function_whose_real_fallback_is_dynamic_is_not_resolved() {
        let code = r#"
package kafka

var Topic = getTopic()

func getTopic() string {
    if v := os.Getenv("TOPIC"); v != "" {
        return v
    }
    if legacy := os.Getenv("LEGACY_TOPIC"); legacy != "" {
        return "legacy-orders"
    }
    return computeTopicFromConfig()
}

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: kafka.Topic,
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations.producers.is_empty(),
            "the function's real unconditional fallback is dynamic (computeTopicFromConfig()), \
             not the intermediate 'legacy-orders' branch — must not resolve to it, got: {:?}",
            relations.producers
        );
    }

    /// A `return "literal"` sitting in a `//` comment after the function's
    /// real last statement must never be picked up — comments aren't part
    /// of the AST at all, unlike a raw text scan which can't tell the
    /// difference.
    #[test]
    fn getenv_function_with_return_literal_in_trailing_comment_is_unaffected() {
        let code = r#"
package kafka

var Topic = getTopic()

func getTopic() string {
    if v := os.Getenv("TOPIC"); v != "" {
        return v
    }
    return "orders"
    // TODO: consider return "staging-topic" later
}

func Emit() {
    producer.SendMessage(&sarama.ProducerMessage{
        Topic: kafka.Topic,
    })
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &tree);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "orders"),
            "a commented-out return must not shadow the real fallback 'orders', got: {:?}",
            relations.producers
        );
    }

    #[test]
    fn non_literal_topic_variable_records_nothing() {
        let code = r#"
package consumer

func Consume() {
    reader := kafka.NewReader(kafka.ReaderConfig{
        Topic: topicVar,
    })
    reader.ReadMessage(ctx)
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 1, &tree);
        assert!(
            relations.consumers.is_empty(),
            "expected no fabricated topic from an unresolved variable, got: {:?}",
            relations.consumers
        );
    }

    #[test]
    fn confluent_subscribe_topics_literal_list_is_detected() {
        let code = r#"
package consumer

func Consume() {
    consumer.SubscribeTopics([]string{"orders", "payments"}, nil)
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 1, &tree);
        assert!(relations
            .consumers
            .iter()
            .any(|(_, t)| t.as_str() == "orders"));
        assert!(relations
            .consumers
            .iter()
            .any(|(_, t)| t.as_str() == "payments"));
    }
    // gRPC registration and router parity tests

    #[test]
    fn grpc_server_registration_emits_grpc_service_and_method_nodes() {
        let code = r#"
package main

type server struct{}

func (s *server) GetUser(ctx context.Context, req *GetUserRequest) (*GetUserResponse, error) {
    return nil, nil
}

func main() {
    pb.RegisterUserServiceServer(grpcServer, &server{})
}
"#;
        let mut p = parser();
        let nodes = GoExtractor::extract(Path::new("main.go"), code, 1, &mut p);

        assert!(
            nodes
                .iter()
                .any(|n| n.kind == NodeKind::GrpcService && n.name == "UserService"),
            "expected a GrpcService node named UserService, got: {nodes:?}"
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == NodeKind::GrpcMethod && n.name == "GetUser"),
            "expected GetUser to be classified as GrpcMethod, got: {nodes:?}"
        );
    }

    /// A `pb.NewFooServiceClient(conn)` call site is recorded as an RPC call
    /// attributed to its enclosing function, enabling CallsRpc resolution.
    #[test]
    fn grpc_client_construction_is_recorded_as_an_rpc_call() {
        let code = r#"
package main

func (fe *frontendServer) placeOrder(w http.ResponseWriter, r *http.Request) {
    client := pb.NewCheckoutServiceClient(fe.checkoutSvcConn)
    client.PlaceOrder(ctx, req)
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (nodes, relations) =
            GoExtractor::extract_with_relations(Path::new("handlers.go"), code, 1, &tree);

        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "placeOrder")
            .expect("enclosing function node present");

        assert!(
            relations
                .rpc_calls
                .iter()
                .any(|(idx, target)| *idx == caller_idx && target.as_str() == "CheckoutService"),
            "expected an rpc_calls entry attributing CheckoutService to placeOrder, got: {:?}",
            relations.rpc_calls
        );
    }

    #[test]
    fn chi_style_route_is_recognized() {
        let code = r#"
package main

func main() {
    r := chi.NewRouter()
    r.Get("/health", healthHandler)
}
"#;
        let mut p = parser();
        let nodes = GoExtractor::extract(Path::new("main.go"), code, 1, &mut p);
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == NodeKind::HttpEndpoint && n.name == "GET /health"),
            "expected a chi-style HttpEndpoint node, got: {nodes:?}"
        );
    }

    #[test]
    fn gin_style_route_is_recognized() {
        let code = r#"
package main

func main() {
    router := gin.Default()
    router.GET("/users/:id", getUser)
}
"#;
        let mut p = parser();
        let nodes = GoExtractor::extract(Path::new("main.go"), code, 1, &mut p);
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == NodeKind::HttpEndpoint && n.name == "GET /users/:id"),
            "expected a gin-style HttpEndpoint node, got: {nodes:?}"
        );
    }

    #[test]
    fn echo_style_route_is_recognized() {
        let code = r#"
package main

func main() {
    e := echo.New()
    e.POST("/orders", createOrder)
}
"#;
        let mut p = parser();
        let nodes = GoExtractor::extract(Path::new("main.go"), code, 1, &mut p);
        assert!(
            nodes
                .iter()
                .any(|n| n.kind == NodeKind::HttpEndpoint && n.name == "POST /orders"),
            "expected an echo-style HttpEndpoint node, got: {nodes:?}"
        );
    }

    #[test]
    fn handle_func_without_http_param_is_not_http_endpoint() {
        let code = r#"
package main

func HandlePanic(err error) {
    println(err)
}

func HandleUser(w http.ResponseWriter, r *http.Request) {
    w.Write([]byte("ok"))
}
"#;
        let mut p = parser();
        let nodes = GoExtractor::extract(Path::new("main.go"), code, 1, &mut p);
        let panic_node = nodes
            .iter()
            .find(|n| n.name == "HandlePanic")
            .expect("HandlePanic");
        assert_eq!(panic_node.kind, NodeKind::ServiceClass);
        let user_node = nodes
            .iter()
            .find(|n| n.name == "HandleUser")
            .expect("HandleUser");
        assert_eq!(user_node.kind, NodeKind::HttpEndpoint);
    }

    #[test]
    fn redis_new_client_does_not_emit_grpc_rpc_call() {
        let code = r#"
package main

func Setup() {
    client := redis.NewClient(&redis.Options{})
    _ = client
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("main.go"), code, 1, &tree);
        assert!(
            relations.rpc_calls.is_empty(),
            "redis.NewClient must not be treated as a gRPC RPC call: {:?}",
            relations.rpc_calls
        );
    }
}
