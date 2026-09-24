use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct GoExtractor;

/// Import/producer/consumer relations extracted alongside `nodes`, expressed as
/// `(local index into the returned node vec, target string)` pairs — the exact
/// shape `mesh_parsers::languages::FileIndex`'s `dependencies`/`producers`/`consumers`
/// fields expect (see `crates/mesh-parsers/src/languages/mod.rs`).
///
/// `PolyglotIndexer::extract`'s `LanguageKind::Go` branch does not thread these
/// through yet — it only takes `extract`'s `Vec<ContractNode>`. Wiring it up is a
/// small, additive change mirroring the existing Java branch (which maps
/// `NodeKind::KafkaTopic` nodes into `.consumers`) or the TypeScript branch (which
/// takes an imports out-param): thread `extract_with_relations`'s second return
/// value straight into `out.dependencies` / `out.producers` / `out.consumers`.
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
    /// Extracts declaration nodes only. Kept at its original arity so existing
    /// call sites (`PolyglotIndexer::extract`'s `LanguageKind::Go` branch) do not
    /// need to change. Still picks up the richer node set from item 5 (gRPC
    /// registrations, router-declared endpoints) since those are ordinary
    /// `ContractNode`s, not out-of-band relations.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        Self::extract_with_relations(file_path, content, repo_id, parser).0
    }

    /// Same extraction as `extract`, plus import/event relations for items 2 and 3.
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> (Vec<ContractNode>, GoRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut relations = GoRelations::default();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return (nodes, relations),
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = CompactStr::default();

        // Pass 1: which struct types were passed as the `srv` argument of a
        // `RegisterXServer(registrar, srv)` call, so method declarations on that
        // type can be classified as `GrpcMethod` rather than plain `ServiceClass`.
        let mut grpc_server_types: HashSet<String> = HashSet::new();
        Self::collect_grpc_server_types(root, source_bytes, &mut grpc_server_types, 0);

        // Pass 1b: top-level `var`/`const` string literal assignments, so a
        // Kafka topic field referencing one by name (`kafka.Topic`) can
        // resolve to the real topic string instead of the raw Go expression.
        let mut string_consts: HashMap<String, CompactStr> = HashMap::new();
        Self::collect_string_consts(root, source_bytes, &mut string_consts, 0);

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
            &string_consts,
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
        string_consts: &HashMap<String, CompactStr>,
        raw_imports: &mut Vec<RawImport>,
        raw_events: &mut Vec<RawEvent>,
        raw_rpc_calls: &mut Vec<RawRpcCall>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        match node.kind() {
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

                let mut kind = NodeKind::ServiceClass;
                if receiver_type
                    .as_deref()
                    .is_some_and(|rt| grpc_server_types.contains(rt))
                {
                    kind = NodeKind::GrpcMethod;
                } else if func_name.starts_with("Handle") || func_name.ends_with("Handler") {
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

    // -- Item 2: imports -----------------------------------------------------

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

    // -- Item 3: kafka-go / sarama / confluent-kafka-go producers/consumers --

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
        if method.starts_with("New") && method.ends_with("Client") {
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

        // kafka-go / sarama / confluent-kafka-go producer/consumer calls. Topic
        // literals are extracted when present (e.g. `ConsumePartition("orders", ...)`,
        // `SubscribeTopics([]string{"orders"})`); otherwise the receiver's
        // identifier is emitted so the agent can still see a topic is used here,
        // rather than silently dropping the signal (RFC roadmap item 3).
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
            if literals.is_empty() {
                let receiver = func
                    .child_by_field_name("operand")
                    .and_then(|o| o.utf8_text(source).ok())
                    .unwrap_or("event");
                raw_events.push(RawEvent {
                    line_start,
                    line_end,
                    topic: CompactStr::new(receiver),
                    is_producer: is_producer_call,
                });
            } else {
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
            if key_text != "Topic" {
                continue;
            }
            let value = elem.child_by_field_name("value")?;
            return Self::extract_topic_value_text(value, source, string_consts);
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
        // Non-literal topic: try resolving it as a reference to a top-level
        // `var`/`const` string assignment (bare `Topic`, or `pkg.Topic` via a
        // selector expression — both keyed on the bare field name in
        // `string_consts`) before falling back to raw expression text. A
        // composite literal's field value is wrapped in a `literal_element`
        // node (`Topic: kafka.Topic` -> `value: (literal_element
        // (selector_expression ...))`), so unwrap that first.
        let unwrapped = if value.kind() == "literal_element" {
            value.named_child(0).unwrap_or(value)
        } else {
            value
        };
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
        // Constant, config lookup, or `&topicVar` we couldn't resolve: still
        // emit something rather than silently dropping it.
        value
            .utf8_text(source)
            .ok()
            .map(|t| CompactStr::new(t.trim_start_matches('&').trim()))
    }

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
    fn collect_string_consts(
        node: Node,
        source: &[u8],
        out: &mut HashMap<String, CompactStr>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if matches!(node.kind(), "var_spec" | "const_spec") {
            // `value` is an `expression_list` wrapping the actual
            // expression(s) — single-name-single-value is the only shape
            // handled (`var X, Y = "a", "b"` is out of scope).
            let single_value = node
                .child_by_field_name("value")
                .filter(|v| v.named_child_count() == 1)
                .and_then(|v| v.named_child(0));
            if let (Some(name_node), Some(value_node)) =
                (node.child_by_field_name("name"), single_value)
            {
                if value_node.kind() == "interpreted_string_literal" {
                    if let (Ok(name), Ok(value)) =
                        (name_node.utf8_text(source), value_node.utf8_text(source))
                    {
                        let unquoted = value.trim_matches('"');
                        if !unquoted.is_empty() {
                            out.insert(name.to_string(), CompactStr::new(unquoted));
                        }
                    }
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_string_consts(child, source, out, depth + 1);
        }
    }

    // -- Item 5: gRPC registration ------------------------------------------

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

    // -- Item 2: imports -> FileIndex.dependencies ---------------------------

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
        let (consumer_nodes, relations) =
            GoExtractor::extract_with_relations(Path::new("main.go"), consumer_code, 1, &mut p2);
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("main.go"), code, 1, &mut p);
        assert!(relations
            .dependencies
            .iter()
            .any(|(_, target)| target.as_str() == "github.com/lib/pq"));
    }

    // -- Item 3: kafka-go / sarama producer<->consumer linking ---------------

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
        let (producer_nodes, producer_relations) = GoExtractor::extract_with_relations(
            Path::new("producer.go"),
            producer_code,
            1,
            &mut p1,
        );
        assert!(!producer_relations.producers.is_empty());

        let mut p2 = parser();
        let (consumer_nodes, consumer_relations) = GoExtractor::extract_with_relations(
            Path::new("consumer.go"),
            consumer_code,
            1,
            &mut p2,
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &mut p);
        assert!(relations
            .producers
            .iter()
            .any(|(_, topic)| topic.as_str() == "payments"));
    }

    /// Regression test for the OpenTelemetry Demo benchmark finding: a
    /// `sarama.ProducerMessage{Topic: kafka.Topic}` literal referencing a
    /// package-level `var Topic = "orders"` constant must resolve to the
    /// real topic name, not the raw Go expression text `"kafka.Topic"` —
    /// `analyze_impact("orders")`, the name a human would actually use,
    /// previously returned nothing for this extremely common shape.
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &mut p);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "orders"),
            "expected the resolved topic 'orders', got: {:?}",
            relations.producers
        );
    }

    /// Documents the explicit non-goal: a topic resolved through a function
    /// call's return value (not a direct `var X = "literal"`) is NOT
    /// resolved — that would require real control-flow interpretation. Must
    /// still fall back to raw expression text rather than silently dropping
    /// the producer signal entirely.
    #[test]
    fn sarama_topic_referencing_a_function_call_falls_back_to_raw_text() {
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("producer.go"), code, 1, &mut p);
        assert!(
            relations
                .producers
                .iter()
                .any(|(_, topic)| topic.as_str() == "kafka.Topic"),
            "a function-call-resolved const is an explicit non-goal — must \
             still fall back to raw expression text, got: {:?}",
            relations.producers
        );
    }

    #[test]
    fn non_literal_topic_emits_variable_name_instead_of_dropping() {
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 1, &mut p);
        assert!(
            relations
                .consumers
                .iter()
                .any(|(_, topic)| topic.as_str() == "topicVar"),
            "expected the non-literal topic variable name to surface, got: {:?}",
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
        let (_, relations) =
            GoExtractor::extract_with_relations(Path::new("consumer.go"), code, 1, &mut p);
        assert!(relations
            .consumers
            .iter()
            .any(|(_, t)| t.as_str() == "orders"));
        assert!(relations
            .consumers
            .iter()
            .any(|(_, t)| t.as_str() == "payments"));
    }

    // -- Item 5: gRPC registration + router parity ---------------------------

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

    /// Regression test for the Online Boutique benchmark finding: a
    /// `pb.NewFooServiceClient(conn)` call site (the standard
    /// `protoc-gen-go-grpc` client constructor — e.g. `frontend` calling
    /// `checkoutservice`) must be recorded as an RPC call attributed to its
    /// enclosing function, so `ContractGraph::reconcile_edges` can build a
    /// real `CallsRpc` edge and `find_dependents`/`analyze_grpc` stop
    /// returning a false negative for a real cross-service caller.
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
        let (nodes, relations) =
            GoExtractor::extract_with_relations(Path::new("handlers.go"), code, 1, &mut p);

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
}
