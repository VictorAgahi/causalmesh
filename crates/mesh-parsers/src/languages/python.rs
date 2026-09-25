use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

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
    /// `(caller node index, target RPC service name)` — a
    /// `grpc_tools.protoc`-generated `<Service>Stub(channel)` construction
    /// site, attributed to its smallest enclosing declaration. Feeds
    /// `ContractGraph::add_rpc_call` -> `reconcile_edges`'s `CallsRpc` edges,
    /// the same client-side signal Go/TypeScript/C#/Kotlin already emit —
    /// Python had none at all, so a Python gRPC client was invisible to
    /// `find_dependents`/`analyze_grpc` even when the service it called was
    /// indexed correctly on the server side.
    pub rpc_calls: Vec<(usize, CompactStr)>,
}

pub struct PythonExtractor;

impl PythonExtractor {
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

    /// Same nodes as `extract`, plus native import dependencies (`import x`,
    /// `from x import y`, relative `from .x import y`) and native event
    /// producer/consumer detection (`confluent_kafka`, `aiokafka`, Celery
    /// `@task` / `@app.task` / `.delay()` / `.apply_async()`).
    pub fn extract_with_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> (Vec<ContractNode>, PythonRelations) {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut imports: Vec<ImportRef> = Vec::new();
        let mut relations = PythonRelations::default();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);

        let mut module_idx: Option<usize> = None;
        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            None,
            root,
            &mut nodes,
            &mut imports,
            &mut relations.producers,
            &mut relations.consumers,
            &mut relations.rpc_calls,
            &mut module_idx,
            0,
        );

        if nodes.is_empty() && !content.trim().is_empty() {
            let stem = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("module");
            let line_count = content.lines().count().max(1);
            nodes.push(ContractNode {
                id: 0,
                name: CompactStr::new(stem),
                kind: NodeKind::ServiceClass,
                file_path: file_path.clone(),
                line_start: 1,
                line_end: line_count,
                package: package_name.clone(),
                repo_id,
                signature: Some(CompactStr::new(format!("module {}", stem))),
                docstring: None,
            });
        }

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
        root: Node,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<ImportRef>,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
        rpc_calls: &mut Vec<(usize, CompactStr)>,
        module_idx: &mut Option<usize>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

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
                // `grpc_tools.protoc` always names its generated gRPC stub
                // module `<proto>_pb2_grpc.py`; it defines a `*Servicer` base
                // class for EVERY service in the shared .proto file, and every
                // service that imports it (to build its own handler
                // elsewhere) vendors an identical copy. Tagging the base class
                // itself as a declared GrpcService node here makes every
                // vendored copy look like a real implementation to
                // `analyze_grpc`'s server-handler search, even for services
                // that never subclass it. The real implementation (a
                // subclass, or the file that instantiates a server with it)
                // lives elsewhere and is picked up on its own merits.
                let is_generated_grpc_stub = file_path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|f| f.ends_with("_pb2_grpc.py"));
                if class_name.ends_with("Servicer") && !is_generated_grpc_stub {
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
                let mut route: Option<(String, String)> = None;
                // Check decorators (supporting multiple stacked decorators, e.g. @app.get + @login_required)
                let mut check_decorator = |dec_node: Node| {
                    if let Ok(dec_text) = dec_node.utf8_text(source) {
                        if Self::is_celery_task_decorator(dec_text) {
                            kind = NodeKind::Queue;
                            celery_task_name = Self::celery_task_name_override(dec_text)
                                .or_else(|| Some(func_name.to_string()));
                        } else if dec_text.contains("@app.")
                            || dec_text.contains("@router.")
                            || dec_text.contains("@api_router.")
                        {
                            kind = NodeKind::HttpEndpoint;
                            if route.is_none() {
                                route = Self::extract_flask_route(dec_node, source);
                            }
                        }
                    }
                };

                if let Some(parent) = node.parent() {
                    if parent.kind() == "decorated_definition" {
                        for i in 0..parent.child_count() {
                            if let Some(child) = parent.child(i) {
                                if child.kind() == "decorator" {
                                    check_decorator(child);
                                }
                            }
                        }
                    }
                }
                // Fallback for sibling decorators
                let mut prev_opt = node.prev_sibling();
                while let Some(prev) = prev_opt {
                    if prev.kind() == "decorator" {
                        check_decorator(prev);
                    } else if prev.kind() != "comment" && prev.kind() != "\n" {
                        break;
                    }
                    prev_opt = prev.prev_sibling();
                }

                let idx = nodes.len();
                if let Some(task_name) = celery_task_name {
                    consumers.push((idx, CompactStr::new(task_name)));
                }

                // A route decorator's actual path/method (`@app.route('/users',
                // methods=['POST'])`, `@router.get("/health")`) used to be
                // discarded entirely — only the Python function name
                // (`create_user`) was kept, with no record anywhere of the
                // real HTTP contract it serves. Surfaced in `signature`
                // rather than replacing `name`, so existing symbol-name
                // lookups (`find_dependents`/`search_symbols`) are unaffected.
                let route_sig = route
                    .as_ref()
                    .map(|(method, path)| format!("{method} {path}"));

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(func_name),
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(route_sig.unwrap_or(first_line))),
                    docstring: None,
                });
                child_enclosing = Some(idx);
            }
            "call" => {
                let idx = enclosing.unwrap_or(0);
                if !nodes.is_empty() {
                    Self::detect_event_call(node, source, idx, producers, consumers, root);
                }
                // Deliberately NOT gated by `!nodes.is_empty()` like the event
                // detection above: a gRPC client is very often wired up at
                // module scope with no prior def/class in the file at all
                // (a minimal script that just imports grpc, opens a channel,
                // and builds a stub) — gating on `nodes` being non-empty
                // would silently drop that shape, defeating the whole point
                // of `module_node_idx` lazily creating one on demand. A
                // real-world example with at least one prior declaration:
                // Online Boutique's recommendationservice builds its
                // ProductCatalogService stub directly inside
                // `if __name__ == "__main__":`, not inside any function.
                // Node 0 (whatever ContractNode happened to be declared
                // first) would be a wrong, arbitrary attribution there.
                let rpc_idx = enclosing.unwrap_or_else(|| {
                    Self::module_node_idx(nodes, file_path, package_name, repo_id, module_idx)
                });
                Self::detect_grpc_stub_call(node, source, rpc_idx, rpc_calls);
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
                root,
                nodes,
                imports,
                producers,
                consumers,
                rpc_calls,
                module_idx,
                depth + 1,
            );
        }
    }

    /// Returns the index of a synthetic module-level `ContractNode`,
    /// creating it (named after the file's stem, e.g. `recommendation_server`
    /// for `recommendation_server.py`) the first time it's actually needed —
    /// most Python files never call this, since most calls happen inside a
    /// function or class the normal walk already attributes them to.
    fn module_node_idx(
        nodes: &mut Vec<ContractNode>,
        file_path: &FilePath,
        package_name: &CompactStr,
        repo_id: RepoId,
        module_idx: &mut Option<usize>,
    ) -> usize {
        if let Some(idx) = *module_idx {
            return idx;
        }
        let stem = file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("module");
        let idx = nodes.len();
        nodes.push(ContractNode {
            id: 0,
            name: CompactStr::new(stem),
            kind: NodeKind::ServiceClass,
            file_path: file_path.clone(),
            line_start: 1,
            line_end: 1,
            package: package_name.clone(),
            repo_id,
            signature: Some(CompactStr::new(format!("module {stem}"))),
            docstring: None,
        });
        *module_idx = Some(idx);
        idx
    }

    /// `grpc_tools.protoc` always names a service's generated client stub
    /// class `<Service>Stub`, in a module itself named `<proto>_pb2_grpc.py`
    /// — constructed at the real call site as
    /// `<proto>_pb2_grpc.<Service>Stub(channel)`, e.g.
    /// `demo_pb2_grpc.ProductCatalogServiceStub(channel)`. Only that
    /// qualified-attribute shape counts: a bare `<Service>Stub(...)` call
    /// with no `_pb2_grpc`-module qualifier is not accepted, because nothing
    /// then distinguishes a real generated stub from a hand-written test
    /// double or mock also named `<Something>Stub` — a common Python testing
    /// idiom that would otherwise fabricate an RPC-call edge to a real
    /// service of the same name.
    fn detect_grpc_stub_call(
        node: Node,
        source: &[u8],
        idx: usize,
        rpc_calls: &mut Vec<(usize, CompactStr)>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "attribute" {
            return;
        }
        let Some(module_obj) = func.child_by_field_name("object") else {
            return;
        };
        let Ok(module_text) = module_obj.utf8_text(source) else {
            return;
        };
        if !module_text.ends_with("_pb2_grpc") {
            return;
        }
        let Some(attr) = func.child_by_field_name("attribute") else {
            return;
        };
        let Ok(name) = attr.utf8_text(source) else {
            return;
        };
        let Some(service) = name.strip_suffix("Stub") else {
            return;
        };
        if service.is_empty() {
            return;
        }
        rpc_calls.push((idx, CompactStr::new(service)));
    }

    // Import extraction

    fn collect_import_names(node: Node, source: &[u8], imports: &mut Vec<ImportRef>) {
        let mut cursor = node.walk();
        for name in node.children_by_field_name("name", &mut cursor) {
            Self::push_name_ref(name, source, true, imports);
        }
    }

    fn collect_import_from(node: Node, source: &[u8], imports: &mut Vec<ImportRef>) {
        if let Some(module) = node.child_by_field_name("module_name") {
            match module.kind() {
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
                            module_like: true,
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

    // Native event producer/consumer detection

    fn detect_event_call(
        node: Node,
        source: &[u8],
        enclosing: usize,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
        root: Node,
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
                        if let Some(topic) = Self::first_arg_value(args, source, "topic", root) {
                            producers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                    "subscribe" => {
                        for topic in Self::list_or_scalar_values(args, source, root) {
                            consumers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                    _ => {}
                }
            }
            "identifier" => {
                if let Ok(name) = func.utf8_text(source) {
                    if name == "AIOKafkaConsumer" {
                        for topic in Self::positional_string_values(args, source, root) {
                            consumers.push((enclosing, CompactStr::new(topic)));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Extracts `(METHOD, path)` from an `@app.route(...)` / `@router.get(...)`
    /// / `@api_router.post(...)`-style decorator's own call expression.
    /// Flask's `@app.route(path, methods=[...])` gets its method from the
    /// `methods` keyword argument (defaulting to `"GET"`, Flask's own
    /// default when it's omitted); FastAPI/typical `APIRouter`-style
    /// `@router.<verb>(path)` gets it directly from the attribute name
    /// (`get`, `post`, ...) instead — a real, different shape from the same
    /// `@app.`/`@router.` prefix this file's caller already gates on.
    fn extract_flask_route(dec_node: Node, source: &[u8]) -> Option<(String, String)> {
        let call = dec_node.named_child(0).filter(|c| c.kind() == "call")?;
        let func = call.child_by_field_name("function")?;
        if func.kind() != "attribute" {
            return None;
        }
        let verb = func
            .child_by_field_name("attribute")
            .and_then(|a| a.utf8_text(source).ok())?;

        let args = call.child_by_field_name("arguments")?;
        let mut positional_path = None;
        let mut rule_kwarg_path = None;
        let mut methods_kwarg: Vec<String> = Vec::new();
        let mut cursor = args.walk();
        for arg in args.named_children(&mut cursor) {
            match arg.kind() {
                "string" if positional_path.is_none() => {
                    positional_path = Self::string_literal_value(arg, source);
                }
                "keyword_argument" => {
                    let kwarg_name = arg
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok());
                    match kwarg_name {
                        // Flask's `@app.route(rule='/x')` — `rule` is a
                        // legitimate named parameter, not just positional.
                        Some("rule") => {
                            rule_kwarg_path = arg
                                .child_by_field_name("value")
                                .and_then(|v| Self::string_literal_value(v, source));
                        }
                        Some("methods") => {
                            if let Some(list) = arg.child_by_field_name("value") {
                                if matches!(list.kind(), "list" | "tuple") {
                                    let mut list_cursor = list.walk();
                                    methods_kwarg = list
                                        .named_children(&mut list_cursor)
                                        .filter_map(|s| Self::string_literal_value(s, source))
                                        .collect();
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        let path = positional_path.or(rule_kwarg_path)?;

        let method = if verb.eq_ignore_ascii_case("route") {
            if methods_kwarg.is_empty() {
                "GET".to_string()
            } else {
                // Every declared method, not just the first — a route
                // registered for `methods=['GET', 'POST']` really does
                // accept both; silently keeping only "GET" would misreport
                // its actual HTTP contract.
                methods_kwarg.join(",")
            }
        } else if matches!(
            verb.to_ascii_lowercase().as_str(),
            "get" | "post" | "put" | "delete" | "patch" | "head" | "options"
        ) {
            verb.to_string()
        } else {
            return None;
        };
        Some((method.to_ascii_uppercase(), path))
    }

    fn is_celery_task_decorator(dec_text: &str) -> bool {
        dec_text.contains("@task") || dec_text.contains("@celery") || dec_text.contains(".task")
    }

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

    fn first_arg_value(args: Node, source: &[u8], kw_name: &str, root: Node) -> Option<String> {
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
                            kw_val = Self::resolve_expr_to_string(value, source, root);
                        }
                    }
                }
                continue;
            }
            return Self::resolve_expr_to_string(child, source, root);
        }
        kw_val
    }

    fn positional_string_values(args: Node, source: &[u8], root: Node) -> Vec<String> {
        let mut out = Vec::new();
        let mut cursor = args.walk();
        for child in args.children(&mut cursor) {
            if !child.is_named() || child.kind() == "comment" {
                continue;
            }
            if child.kind() == "keyword_argument" {
                break;
            }
            if let Some(val) = Self::resolve_expr_to_string(child, source, root) {
                out.push(val);
            }
        }
        out
    }

    fn list_or_scalar_values(args: Node, source: &[u8], root: Node) -> Vec<String> {
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
                .filter_map(|c| Self::resolve_expr_to_string(c, source, root))
                .collect()
        } else {
            Self::resolve_expr_to_string(first, source, root)
                .into_iter()
                .collect()
        }
    }

    fn resolve_expr_to_string(node: Node, source: &[u8], root: Node) -> Option<String> {
        if node.kind() == "string" {
            return Self::string_literal_value(node, source);
        }
        if node.kind() == "identifier" || node.kind() == "attribute" {
            let var_name = node.utf8_text(source).ok()?.trim();
            if let Some(val) = Self::find_assignment_in_root(root, source, var_name) {
                return Some(val);
            }
        }
        None
    }

    fn find_assignment_in_root(root: Node, source: &[u8], target_name: &str) -> Option<String> {
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "expression_statement" {
                if let Some(assign) = child.child(0) {
                    if assign.kind() == "assignment" {
                        let left = assign.child_by_field_name("left")?;
                        let right = assign.child_by_field_name("right")?;
                        let left_name = left.utf8_text(source).ok()?.trim();
                        if left_name == target_name && right.kind() == "string" {
                            return Self::string_literal_value(right, source);
                        }
                    }
                }
            } else if child.kind() == "assignment" {
                let left = child.child_by_field_name("left")?;
                let right = child.child_by_field_name("right")?;
                let left_name = left.utf8_text(source).ok()?.trim();
                if left_name == target_name && right.kind() == "string" {
                    return Self::string_literal_value(right, source);
                }
            }
        }
        None
    }

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
    use mesh_core::{ContractGraph, NodeId};

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

    /// A `*Servicer` class defined inside a `grpc_tools.protoc`-generated
    /// `*_pb2_grpc.py` stub is a base class and must NOT be tagged `GrpcService`.
    #[test]
    fn generated_pb2_grpc_servicer_base_class_is_not_tagged_grpc_service() {
        let code = r#"
class CheckoutServiceServicer(object):
    """Missing associated documentation comment in .proto file."""

    def PlaceOrder(self, request, context):
        raise NotImplementedError()
"#;
        let mut parser = make_parser();
        let nodes = PythonExtractor::extract(
            Path::new("src/emailservice/demo_pb2_grpc.py"),
            code,
            3,
            &mut parser,
        );
        let servicer = nodes
            .iter()
            .find(|n| n.name == "CheckoutServiceServicer")
            .expect("class node present");
        assert_eq!(
            servicer.kind,
            NodeKind::ServiceClass,
            "a Servicer base class inside a generated *_pb2_grpc.py stub \
             must not be tagged GrpcService, got: {:?}",
            servicer.kind
        );
    }

    /// A real handler subclassing the generated base outside a `_pb2_grpc.py`
    /// file is unaffected — it's still tagged `GrpcService` as before.
    #[test]
    fn real_servicer_subclass_outside_generated_stub_is_still_tagged_grpc_service() {
        let code = r#"
class CheckoutServiceServicer(checkout_pb2_grpc.CheckoutServiceServicer):
    def PlaceOrder(self, request, context):
        return do_checkout(request)
"#;
        let mut parser = make_parser();
        let nodes = PythonExtractor::extract(
            Path::new("src/checkoutservice/main.py"),
            code,
            3,
            &mut parser,
        );
        let servicer = nodes
            .iter()
            .find(|n| n.name == "CheckoutServiceServicer")
            .expect("class node present");
        assert_eq!(servicer.kind, NodeKind::GrpcService);
    }

    /// `<Service>Stub(channel)` — a `grpc_tools.protoc`-generated client
    /// stub's real construction site — must be recorded as an RPC call to
    /// `<Service>`. Caught empirically scoring `mesh-mcp` against the real
    /// Online Boutique demo (`tests/golden/online-boutique.expected.yaml`):
    /// `recommendationservice`'s `demo_pb2_grpc.ProductCatalogServiceStub(channel)`
    /// was invisible to `find_dependents`/`analyze_grpc` before this fix.
    #[test]
    fn grpc_stub_construction_is_recorded_as_an_rpc_call() {
        let code = r#"
def get_product_catalog_stub(channel):
    return demo_pb2_grpc.ProductCatalogServiceStub(channel)

def list_recommendations(product_catalog_stub):
    return product_catalog_stub.ListProducts(demo_pb2.Empty())
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("src/recommendationservice/recommendation_server.py"),
            code,
            0,
            &tree,
        );
        assert_eq!(
            relations.rpc_calls.len(),
            1,
            "expected exactly 1 rpc call, got: {:?}",
            relations.rpc_calls
        );
        assert_eq!(relations.rpc_calls[0].1.as_str(), "ProductCatalogService");
        assert_eq!(
            nodes[relations.rpc_calls[0].0].name.as_str(),
            "get_product_catalog_stub"
        );
    }

    /// A bare `<Something>Stub(...)` construction with no `_pb2_grpc`-module
    /// qualifier must NOT be recorded — a common Python testing idiom is a
    /// hand-written fake named after the real service it doubles for (e.g.
    /// `class PaymentServiceStub: ...` as a test double), and nothing
    /// distinguishes that from a real generated client without requiring the
    /// qualified `<proto>_pb2_grpc.<Service>Stub` shape.
    #[test]
    fn bare_stub_construction_without_pb2_grpc_qualifier_records_nothing() {
        let code = r#"
def make_fake_payment_client():
    return PaymentServiceStub()
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (_, relations) =
            PythonExtractor::extract_with_relations(Path::new("tests/fakes.py"), code, 0, &tree);
        assert!(
            relations.rpc_calls.is_empty(),
            "expected no rpc call from an unqualified Stub-named construction, got: {:?}",
            relations.rpc_calls
        );
    }

    /// A module-level stub construction (no enclosing function/class) has no
    /// legitimate node to attribute the call to and must be skipped, not
    /// mis-attributed to whichever `ContractNode` happens to be declared
    /// first in the file.
    #[test]
    fn module_level_stub_construction_attributes_to_a_module_node_not_the_first_unrelated_one() {
        let code = r#"
class Unrelated:
    pass

if __name__ == "__main__":
    channel = grpc.insecure_channel("localhost:50051")
    stub = demo_pb2_grpc.ProductCatalogServiceStub(channel)
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("recommendation_server.py"),
            code,
            0,
            &tree,
        );
        assert_eq!(
            relations.rpc_calls.len(),
            1,
            "expected 1 rpc call, got: {:?}",
            relations.rpc_calls
        );
        let (idx, target) = &relations.rpc_calls[0];
        assert_eq!(target.as_str(), "ProductCatalogService");
        assert_ne!(
            nodes[*idx].name.as_str(),
            "Unrelated",
            "must not be misattributed to the first unrelated node"
        );
        assert_eq!(nodes[*idx].name.as_str(), "recommendation_server");
    }

    /// End-to-end: a Python client's `<Service>Stub(channel)` construction
    /// must actually resolve to a real `CallsRpc` edge through
    /// `ContractGraph::reconcile_edges` — not just produce the right raw
    /// `(idx, target)` tuple in isolation, which alone wouldn't catch a
    /// resolution-side naming mismatch. The server side here is a
    /// `GrpcService` node added directly, the same shape a `.proto`
    /// declaration produces (`proto.rs`'s extractor) — this is the actual
    /// resolution path the real Online Boutique fix (`docs/quality.md`)
    /// exercises: every golden-corpus repo's services are declared in a real
    /// `.proto` file, not resolved via Python's own `*Servicer` class naming
    /// alone.
    #[test]
    fn grpc_stub_resolves_to_a_real_edge_against_a_proto_declared_service() {
        let mut graph = ContractGraph::new();

        let service_id = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("CheckoutService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("protos/demo.proto").into(),
            line_start: 1,
            line_end: 1,
            package: CompactStr::new("hipstershop"),
            repo_id: 0,
            signature: Some(CompactStr::new("service CheckoutService")),
            docstring: None,
        });

        let client_code = r#"
def place_order(channel):
    stub = checkout_pb2_grpc.CheckoutServiceStub(channel)
    return stub.PlaceOrder(request)
"#;
        let mut parser = make_parser();
        let tree = parser.parse(client_code, None).expect("parse");
        let (client_nodes, client_relations) = PythonExtractor::extract_with_relations(
            Path::new("src/frontend/handlers.py"),
            client_code,
            1,
            &tree,
        );
        let client_ids: Vec<NodeId> = client_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (idx, target) in client_relations.rpc_calls {
            graph.add_rpc_call(client_ids[idx], target.as_str());
        }
        graph.reconcile_edges();

        let caller_id = client_ids[0];
        let has_edge = graph
            .all_edges()
            .iter()
            .any(|e| e.from == caller_id && e.to == service_id);
        assert!(
            has_edge,
            "expected a CallsRpc edge from the CheckoutServiceStub caller to the \
             proto-declared CheckoutService node, found edges: {:?}",
            graph.all_edges()
        );
    }

    /// A gRPC stub construction with zero prior def/class in the file (no
    /// enclosing declaration, and `nodes` still empty when the call is
    /// visited) must still be recorded via the lazily-created module node —
    /// not silently dropped by a stray `!nodes.is_empty()` guard meant only
    /// for the unrelated event-producer/consumer detection.
    #[test]
    fn grpc_stub_with_zero_prior_declarations_is_still_recorded() {
        let code = r#"
import grpc
import demo_pb2_grpc

channel = grpc.insecure_channel("localhost:50051")
stub = demo_pb2_grpc.ProductCatalogServiceStub(channel)
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, relations) =
            PythonExtractor::extract_with_relations(Path::new("client.py"), code, 0, &tree);
        assert_eq!(
            relations.rpc_calls.len(),
            1,
            "expected 1 rpc call even with zero prior declarations, got: {:?}",
            relations.rpc_calls
        );
        assert_eq!(relations.rpc_calls[0].1.as_str(), "ProductCatalogService");
        assert_eq!(nodes[relations.rpc_calls[0].0].name.as_str(), "client");
    }

    /// Plain `from x import y`: a symbol declared in one file and
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

        let consumer_tree = parser.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/main.py"),
            consumer_code,
            1,
            &consumer_tree,
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

    /// Relative `from .x import y`: imports resolved through a relative path.
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

        let consumer_tree = parser.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/main.py"),
            consumer_code,
            1,
            &consumer_tree,
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

    /// Confluent-kafka / aiokafka: a `.produce()` call in one file
    /// and a `.subscribe()` loop in another are linked through `analyze_impact`.
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

        let producer_tree = parser.parse(producer_code, None).expect("parse");
        let (producer_nodes, producer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer.py"),
            producer_code,
            1,
            &producer_tree,
        );
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in producer_relations.producers {
            graph.add_producer(producer_ids[i], topic.as_str());
        }

        let consumer_tree = parser.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, consumer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/consumer.py"),
            consumer_code,
            1,
            &consumer_tree,
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

    /// Celery: `.delay()` as a producer and `@app.task` as a
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

        let producer_tree = parser.parse(producer_code, None).expect("parse");
        let (producer_nodes, producer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/client.py"),
            producer_code,
            1,
            &producer_tree,
        );
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, task) in producer_relations.producers {
            graph.add_producer(producer_ids[i], task.as_str());
        }

        let consumer_tree = parser.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, consumer_relations) = PythonExtractor::extract_with_relations(
            Path::new("services/tasks.py"),
            consumer_code,
            1,
            &consumer_tree,
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
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer.py"),
            code,
            1,
            &tree,
        );
        let idx = nodes
            .iter()
            .position(|n| n.name == "send_event")
            .expect("send_event node");

        assert!(
            relations
                .producers
                .iter()
                .any(|(i, topic)| *i == idx && topic.as_str() == "orders"),
            "expected the resolved constant value 'orders' to be emitted, got: {:?}",
            relations.producers
        );
    }

    /// A variable resolved via `find_assignment_in_root` must only count when
    /// its assignment is a genuine string literal. `string_literal_value`
    /// finds the *first and last quote characters* in a node's raw text —
    /// for a non-string right-hand side like `os.getenv('KAFKA_TOPIC')`,
    /// that raw text still contains quotes, so it used to extract
    /// "KAFKA_TOPIC" (the *env var's key*, from `getenv`'s own argument) as
    /// if it were the resolved topic value, which it never was.
    #[test]
    fn test_python_topic_variable_assigned_from_non_literal_records_nothing() {
        let code = r#"
from confluent_kafka import Producer

TOPIC = os.getenv('KAFKA_TOPIC')

def send_event():
    producer = Producer({})
    producer.produce(TOPIC, b"data")
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (_, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer.py"),
            code,
            1,
            &tree,
        );
        assert!(
            relations.producers.is_empty(),
            "expected no fabricated topic from a non-literal assignment, got: {:?}",
            relations.producers
        );
    }

    #[test]
    fn test_python_script_file_imports() {
        let script_code = r#"
import kafka

producer = kafka.KafkaProducer(bootstrap_servers='localhost:9092')
"#;
        let mut graph = ContractGraph::default();
        let mut parser = make_parser();
        let tree = parser.parse(script_code, None).expect("parse");
        let (nodes, relations) = PythonExtractor::extract_with_relations(
            Path::new("services/producer_script.py"),
            script_code,
            1,
            &tree,
        );
        assert_eq!(nodes.len(), 1, "expected 1 module node for script file");
        assert_eq!(nodes[0].name.as_str(), "producer_script");
        let ids: Vec<_> = nodes.into_iter().map(|n| graph.add_node(n)).collect();
        for (i, target) in relations.dependencies {
            graph.add_dependency(ids[i], target.as_str());
        }

        let dependents = graph.find_dependents("kafka");
        assert!(
            dependents.iter().any(|n| n.name == "producer_script"),
            "expected `producer_script` in find_dependents('kafka'), got: {:?}",
            dependents
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_python_multi_stacked_decorators() {
        let code = r#"
@app.get("/items")
@login_required
@permission_required("admin")
def get_items():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("app/routes.py"), code, 0, &tree);
        let endpoint = nodes
            .iter()
            .find(|n| n.name == "get_items")
            .expect("get_items node");
        assert_eq!(
            endpoint.kind,
            NodeKind::HttpEndpoint,
            "stacked decorators must correctly identify HttpEndpoint"
        );
    }

    /// A `@app.route(path, methods=[...])` decorator's real path and method
    /// used to be discarded entirely — only the Python function name was
    /// kept, with no record anywhere of the actual HTTP contract it serves.
    /// Verbatim shape from the real Bank of Anthos userservice.
    #[test]
    fn flask_route_path_and_method_are_surfaced_in_signature() {
        let code = r#"
@app.route('/users', methods=['POST'])
def create_user():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("userservice.py"), code, 0, &tree);
        let node = nodes
            .iter()
            .find(|n| n.name == "create_user")
            .expect("create_user node");
        assert_eq!(node.kind, NodeKind::HttpEndpoint);
        assert_eq!(node.signature.as_deref(), Some("POST /users"));
    }

    /// `@app.route(path)` with no `methods=` keyword argument defaults to
    /// `GET`, matching Flask's own default.
    #[test]
    fn flask_route_with_no_methods_kwarg_defaults_to_get() {
        let code = r#"
@app.route('/version')
def version():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("userservice.py"), code, 0, &tree);
        let node = nodes
            .iter()
            .find(|n| n.name == "version")
            .expect("version node");
        assert_eq!(node.signature.as_deref(), Some("GET /version"));
    }

    /// A route registered for multiple methods (`methods=['GET', 'POST']`)
    /// really does accept both — only capturing the first would misreport
    /// its actual HTTP contract.
    #[test]
    fn flask_route_with_multiple_methods_keeps_all_of_them() {
        let code = r#"
@app.route('/users/<int:id>', methods=['GET', 'POST'])
def user_detail():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("userservice.py"), code, 0, &tree);
        let node = nodes
            .iter()
            .find(|n| n.name == "user_detail")
            .expect("user_detail node");
        assert_eq!(node.signature.as_deref(), Some("GET,POST /users/<int:id>"));
    }

    /// Flask's `@app.route(rule='/x')` — `rule` is a legitimate named
    /// parameter, not just a positional argument — must still be recognized
    /// as the route path, not silently dropped back to the plain function
    /// signature despite the node still being tagged `HttpEndpoint`.
    #[test]
    fn flask_route_with_keyword_only_rule_argument_is_recognized() {
        let code = r#"
@app.route(rule='/x')
def x_handler():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("app.py"), code, 0, &tree);
        let node = nodes
            .iter()
            .find(|n| n.name == "x_handler")
            .expect("x_handler node");
        assert_eq!(node.signature.as_deref(), Some("GET /x"));
    }

    /// FastAPI/`APIRouter`-style `@router.<verb>(path)` gets its method
    /// directly from the attribute name, not a `methods=` keyword argument —
    /// a genuinely different shape from Flask's `@app.route`, sharing only
    /// the `@app.`/`@router.` prefix this file's caller gates on.
    #[test]
    fn fastapi_router_verb_decorator_is_surfaced_in_signature() {
        let code = r#"
@router.get("/health")
def health_check():
    pass
"#;
        let mut parser = make_parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _) =
            PythonExtractor::extract_with_relations(Path::new("main.py"), code, 0, &tree);
        let node = nodes
            .iter()
            .find(|n| n.name == "health_check")
            .expect("health_check node");
        assert_eq!(node.kind, NodeKind::HttpEndpoint);
        assert_eq!(node.signature.as_deref(), Some("GET /health"));
    }
}
