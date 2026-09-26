use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

pub struct TypeScriptExtractor;

/// Historical hardcoded gRPC decorator, used when `controller_annotations`
/// is not configured.
const DEFAULT_GRPC_ANNOTATION: &str = "@GrpcMethod";

/// A raw NestJS `ClientGrpc.getService<XServiceClient>(...)` call site,
/// found at `(line_start, line_end)`, targeting service `service_name`.
/// Resolved to its smallest enclosing declaration in `resolve_rpc_calls`,
/// mirroring `go.rs`'s `RawRpcCall`/`resolve_rpc_calls` for
/// `pb.NewFooServiceClient(...)` — same "detect the client-construction call
/// site by name, don't track what happens to the value afterward" heuristic.
struct RawRpcCall {
    line_start: usize,
    line_end: usize,
    service_name: CompactStr,
    /// Set only for a generated-client construction bound to a variable
    /// (`const client = new XClient(...)`). ts-proto clients are routinely
    /// built once at module level, where no declaration encloses the call
    /// site — dropping those was the whole otel-demo `frontend -> *` recall
    /// gap (Plan 4 step 4.5). When nothing encloses the construction, the
    /// call is attributed instead to every declaration that *references*
    /// the binding (the methods that actually issue the RPCs), never to a
    /// node synthesized for the purpose: a construction whose target turns
    /// out not to be a declared service (`new S3Client()`,
    /// `new QueryClient()`) then leaves no trace in the graph, because an
    /// unresolved `rpc_calls` entry is simply dropped by `reconcile_edges`.
    binding: Option<ClientBinding>,
}

/// The variable a client construction is bound to.
struct ClientBinding {
    name: String,
    /// Start byte of the declarator's name identifier — the binding site
    /// itself, which is not a reference to it.
    decl_start_byte: usize,
}

/// Every binding a file's top-level `import` statements introduce — the
/// "is this client class actually imported here?" gate for
/// `new XClient(...)` detection. Keyed on the *local* name (what a `new`
/// expression references), mapped to the name as exported by the source
/// module (what identifies the generated client class), so an aliased
/// `import { AdServiceClient as Ads }` still resolves `new Ads(...)`.
#[derive(Default)]
struct ImportBindings {
    /// local name -> exported name (named and default imports).
    named: HashMap<String, String>,
    /// `import * as ns from '...'` local names.
    namespaces: HashSet<String>,
}

/// Per-file invariants threaded through the recursive `visit_node` walk,
/// bundled to keep its argument count down.
struct VisitCtx<'a> {
    file_path: &'a FilePath,
    repo_id: RepoId,
    package_name: &'a CompactStr,
    grpc_annotations: &'a [String],
    import_bindings: &'a ImportBindings,
}

impl TypeScriptExtractor {
    /// Test/ad-hoc entry point using the legacy hardcoded `@GrpcMethod` decorator;
    /// parses `content` itself. Production indexing goes through
    /// [`Self::extract_with_config`] via `PolyglotIndexer`, which parses once with
    /// `AstGuard::parse_with` so a parse failure is visible instead of silently
    /// producing an empty result indistinguishable from a legitimately empty file.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
    ) -> Vec<ContractNode> {
        let Some(tree) = parser.parse(content, None) else {
            return Vec::new();
        };
        let mut rpc_calls = Vec::new();
        Self::extract_with_config(
            file_path,
            content,
            repo_id,
            &tree,
            imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        )
    }

    /// Same as [`Self::extract`], but `grpc_annotations` (from
    /// `[engines.contracts.grpc] controller_annotations`) replaces the
    /// hardcoded `@GrpcMethod` decorator list used to recognise gRPC handlers.
    /// `rpc_calls` is filled with `(local node index, target service name)`
    /// pairs — the same shape `languages::FileIndex.rpc_calls` expects (see
    /// `languages/mod.rs`) — resolved from every
    /// `ClientGrpc.getService<XServiceClient>(...)` call site found.
    #[allow(clippy::too_many_arguments)]
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
        rpc_calls: &mut Vec<(usize, CompactStr)>,
        grpc_annotations: &[String],
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);
        let import_bindings = Self::collect_import_bindings(root, source_bytes);
        let ctx = VisitCtx {
            file_path: &file_path,
            repo_id,
            package_name: &package_name,
            grpc_annotations,
            import_bindings: &import_bindings,
        };

        let mut raw_rpc_calls: Vec<RawRpcCall> = Vec::new();
        Self::visit_node(
            root,
            source_bytes,
            &ctx,
            &mut nodes,
            imports,
            &mut raw_rpc_calls,
            0,
        );
        Self::resolve_rpc_calls(&nodes, raw_rpc_calls, rpc_calls, root, source_bytes);
        nodes
    }

    /// Attaches each raw RPC call site to its smallest enclosing declaration,
    /// mirroring `go.rs::resolve_rpc_calls`. A call site with no enclosing
    /// declaration has no sensible caller of its own: a `getService<...>(...)`
    /// one is dropped, and a bound `const client = new XClient(...)` one is
    /// attributed to the declarations referencing `client` (see
    /// [`RawRpcCall::binding`]) — or dropped if there are none. No node is
    /// ever created here.
    fn resolve_rpc_calls(
        nodes: &[ContractNode],
        raw_rpc_calls: Vec<RawRpcCall>,
        out: &mut Vec<(usize, CompactStr)>,
        root: Node,
        source: &[u8],
    ) {
        for call in raw_rpc_calls {
            if let Some(idx) = Self::smallest_enclosing(nodes, call.line_start, call.line_end) {
                out.push((idx, call.service_name));
                continue;
            }
            let Some(binding) = call.binding else {
                continue;
            };
            // Ordered set: one entry per referencing declaration, in node
            // order, so the output is independent of reference order.
            let mut callers = BTreeSet::new();
            for line in Self::binding_reference_lines(root, source, &binding) {
                if let Some(idx) = Self::smallest_enclosing(nodes, line, line) {
                    callers.insert(idx);
                }
            }
            for idx in callers {
                out.push((idx, call.service_name.clone()));
            }
        }
    }

    /// Index of the smallest declaration whose line range covers
    /// `line_start..=line_end`.
    fn smallest_enclosing(
        nodes: &[ContractNode],
        line_start: usize,
        line_end: usize,
    ) -> Option<usize> {
        nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.line_start <= line_start && n.line_end >= line_end)
            .min_by_key(|(_, n)| n.line_end - n.line_start)
            .map(|(idx, _)| idx)
    }

    /// 1-based lines of every reference to `binding` in the file: an
    /// `identifier` (`client.getAds(...)`) or object shorthand
    /// (`{ client }`) spelling its name, other than the binding site itself.
    /// Property names (`this.client`, `x.client`) are `property_identifier`
    /// nodes and never match. Name-based, not scope-resolved: a local that
    /// shadows the binding counts as a reference too, which can only ever
    /// attribute the call to another declaration of the same file.
    /// Iterative walk, so no recursion-depth concern on deep files.
    fn binding_reference_lines(root: Node, source: &[u8], binding: &ClientBinding) -> Vec<usize> {
        let mut lines = Vec::new();
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
                && node.start_byte() != binding.decl_start_byte
                && node.utf8_text(source).is_ok_and(|t| t == binding.name)
            {
                lines.push(node.start_position().row + 1);
            }
            if cursor.goto_first_child() || cursor.goto_next_sibling() {
                continue;
            }
            loop {
                if !cursor.goto_parent() {
                    return lines;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// Pre-pass over the file's top-level `import` statements (ES modules
    /// only allow them there) collecting every local binding they introduce.
    fn collect_import_bindings(root: Node, source: &[u8]) -> ImportBindings {
        let mut bindings = ImportBindings::default();
        let mut cursor = root.walk();
        for stmt in root.children(&mut cursor) {
            if stmt.kind() != "import_statement" {
                continue;
            }
            let mut stmt_cursor = stmt.walk();
            for clause in stmt.children(&mut stmt_cursor) {
                if clause.kind() != "import_clause" {
                    continue;
                }
                let mut clause_cursor = clause.walk();
                for part in clause.children(&mut clause_cursor) {
                    match part.kind() {
                        // `import AdServiceClient from '...'`
                        "identifier" => {
                            if let Ok(name) = part.utf8_text(source) {
                                bindings.named.insert(name.to_string(), name.to_string());
                            }
                        }
                        // `import * as demo from '...'`
                        "namespace_import" => {
                            let mut ns_cursor = part.walk();
                            for child in part.named_children(&mut ns_cursor) {
                                if child.kind() == "identifier" {
                                    if let Ok(name) = child.utf8_text(source) {
                                        bindings.namespaces.insert(name.to_string());
                                    }
                                }
                            }
                        }
                        // `import { AdServiceClient, CartServiceClient as Cart } from '...'`
                        "named_imports" => {
                            let mut spec_cursor = part.walk();
                            for spec in part.children(&mut spec_cursor) {
                                if spec.kind() != "import_specifier" {
                                    continue;
                                }
                                let Some(exported) = spec
                                    .child_by_field_name("name")
                                    .and_then(|n| n.utf8_text(source).ok())
                                else {
                                    continue;
                                };
                                let local = spec
                                    .child_by_field_name("alias")
                                    .and_then(|n| n.utf8_text(source).ok())
                                    .unwrap_or(exported);
                                bindings
                                    .named
                                    .insert(local.to_string(), exported.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        bindings
    }

    /// Resolves a `new_expression`'s constructor to the gRPC service it
    /// names, if and only if the constructed class is an *imported*
    /// `<X>Client`: `new AdServiceClient(...)` with `AdServiceClient`
    /// imported (named, aliased or default), or `new demo.AdServiceClient(...)`
    /// with `demo` an imported binding. Returns `<X>` — deliberately not
    /// checked against anything else here: whether `<X>` (or `<X>Service`)
    /// is a real gRPC service is decided by `ContractGraph::reconcile_edges`
    /// against the services actually declared in the graph, which is what
    /// keeps an arbitrary imported `new S3Client()` from becoming an edge.
    /// No import-path heuristic is involved (a path containing `proto` says
    /// nothing: `prototype`, `protocol`, ...).
    fn extract_client_construction_target(
        ctor: Node,
        ctx: &VisitCtx,
        source: &[u8],
    ) -> Option<String> {
        let class_name = match ctor.kind() {
            "identifier" => {
                let local = ctor.utf8_text(source).ok()?;
                ctx.import_bindings.named.get(local)?.as_str()
            }
            "member_expression" => {
                let object = ctor.child_by_field_name("object")?;
                if object.kind() != "identifier" {
                    return None;
                }
                let object_name = object.utf8_text(source).ok()?;
                if !ctx.import_bindings.namespaces.contains(object_name)
                    && !ctx.import_bindings.named.contains_key(object_name)
                {
                    return None;
                }
                ctor.child_by_field_name("property")?
                    .utf8_text(source)
                    .ok()?
            }
            _ => return None,
        };
        let service = class_name.strip_suffix("Client")?;
        // Generated client classes are PascalCase (`AdServiceClient`);
        // this also rejects a bare `Client` (empty `<X>`).
        if !service.starts_with(|c: char| c.is_ascii_uppercase()) {
            return None;
        }
        Some(service.to_string())
    }

    /// The variable a client construction is directly bound to
    /// (`const client = new XClient(...)`), if any.
    fn client_binding(node: Node, source: &[u8]) -> Option<ClientBinding> {
        let name = node
            .parent()
            .filter(|p| p.kind() == "variable_declarator")?
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")?;
        Some(ClientBinding {
            name: name.utf8_text(source).ok()?.to_string(),
            decl_start_byte: name.start_byte(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        ctx: &VisitCtx,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<(String, String)>,
        raw_rpc_calls: &mut Vec<RawRpcCall>,
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
                let from_str_opt = node
                    .child_by_field_name("source")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(|s| {
                        s.trim()
                            .trim_matches(';')
                            .trim()
                            .trim_matches('\'')
                            .trim_matches('"')
                    })
                    .or_else(|| {
                        node.utf8_text(source).ok().and_then(|text| {
                            text.rfind("from").map(|from_idx| {
                                text[from_idx + 4..]
                                    .trim()
                                    .trim_matches(';')
                                    .trim()
                                    .trim_matches('\'')
                                    .trim_matches('"')
                            })
                        })
                    });

                if let Some(from_str) = from_str_opt {
                    if !from_str.is_empty() {
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
                    || full_text.contains("@Delete")
                    || full_text.contains("@Patch")
                    || full_text.contains("@Options")
                    || full_text.contains("@Head")
                    || full_text.contains("@All")
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
            // NestJS: `this.client.getService<FooServiceClient>('FooService')`
            // (or the string-literal argument alone, when no generic type
            // argument is present) — the standard `ClientGrpc` client
            // construction call. Recorded here (raw, line-range only) and
            // attributed to its smallest enclosing declaration in
            // `resolve_rpc_calls`, mirroring `go.rs`'s `NewFooServiceClient`
            // detection: the call site itself is sufficient evidence this
            // code talks to the named service — what happens to the
            // returned value afterward is not tracked.
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    if callee.kind() == "member_expression" {
                        if let Some(prop) = callee.child_by_field_name("property") {
                            if let Ok(prop_name) = prop.utf8_text(source) {
                                if prop_name == "getService" {
                                    if let Some(service_name) =
                                        Self::extract_get_service_target(node, source)
                                    {
                                        raw_rpc_calls.push(RawRpcCall {
                                            line_start: node.start_position().row + 1,
                                            line_end: node.end_position().row + 1,
                                            service_name: CompactStr::new(service_name),
                                            binding: None,
                                        });
                                    }
                                }
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
            // ts-proto / grpc-js generated clients: `new AdServiceClient(addr, creds)`
            // with `AdServiceClient` imported is a client-construction call
            // site, recorded like `getService<...>` above (Plan 4 step 4.5).
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    if let Some(service_name) =
                        Self::extract_client_construction_target(ctor, ctx, source)
                    {
                        raw_rpc_calls.push(RawRpcCall {
                            line_start: node.start_position().row + 1,
                            line_end: node.end_position().row + 1,
                            service_name: CompactStr::new(service_name),
                            binding: Self::client_binding(node, source),
                        });
                    }
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
            Self::visit_node(child, source, ctx, nodes, imports, raw_rpc_calls, depth + 1);
        }
    }

    /// Reads the service name off a `getService<XServiceClient>('XService')`
    /// call: prioritizes the canonical string-literal argument representing the
    /// exact proto contract, falling back to the generic type argument when no
    /// string literal is present (stripping namespace, `Client`/`Stub` suffix,
    /// and `I` interface prefix).
    fn extract_get_service_target(node: Node, source: &[u8]) -> Option<String> {
        // 1. Canonical string-literal argument (exact proto service name)
        if let Some(args) = node.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for arg in args.named_children(&mut cursor) {
                if arg.kind() == "string" {
                    if let Ok(text) = arg.utf8_text(source) {
                        let trimmed =
                            text.trim_matches(|c: char| c == '\'' || c == '"' || c == '`');
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
            }
        }

        // 2. Generic type argument fallback (e.g. proto.checkout.ICheckoutServiceClient -> CheckoutService)
        if let Some(type_args) = node.child_by_field_name("type_arguments") {
            let mut cursor = type_args.walk();
            for child in type_args.named_children(&mut cursor) {
                if let Ok(text) = child.utf8_text(source) {
                    let bare = text.rsplit('.').next().unwrap_or(text);
                    let without_suffix = bare
                        .strip_suffix("Client")
                        .or_else(|| bare.strip_suffix("Stub"))
                        .unwrap_or(bare);
                    let clean_name = if without_suffix.starts_with('I')
                        && without_suffix
                            .chars()
                            .nth(1)
                            .is_some_and(|c| c.is_uppercase())
                    {
                        &without_suffix[1..]
                    } else {
                        without_suffix
                    };
                    if !clean_name.is_empty() {
                        return Some(clean_name.to_string());
                    }
                }
            }
        }
        None
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

    /// Unquotes a string-literal node's text. Anything else (an identifier, a
    /// member expression like `config.topic`, a template literal with
    /// interpolation, ...) returns `None` instead of the raw expression
    /// text: this extractor has no constant-propagation pass to resolve a
    /// variable/config-lookup against, so its source text — `"config.topic"`
    /// or `` "`orders-${env}`" `` verbatim — is not a topic name, just
    /// whatever expression happened to be written at that call site.
    fn extract_value_text(value_node: Node, source: &[u8]) -> Option<String> {
        if value_node.kind() != "string" {
            return None;
        }
        let text = value_node.utf8_text(source).ok()?;
        Some(
            text.trim_matches('\'')
                .trim_matches('"')
                .trim_matches('`')
                .to_string(),
        )
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
        let mut rpc_calls2 = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let configured_nodes = TypeScriptExtractor::extract_with_config(
            Path::new("auth.controller.ts"),
            code,
            4,
            &tree,
            &mut imports2,
            &mut rpc_calls2,
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
    fn test_kafkajs_non_literal_topic_records_nothing() {
        let code = r#"
async function run() {
    await consumer.subscribe({ topic: TOPIC_NAME });
}
"#;
        let (nodes, _) = parse(code);

        // `TOPIC_NAME` is a variable, not the topic value; this extractor has
        // no constant-propagation pass to resolve it against, so it used to
        // fabricate a KafkaTopic node named after the variable itself.
        assert!(
            !nodes.iter().any(|n| n.kind == NodeKind::KafkaTopic),
            "a non-literal topic argument must not fabricate a KafkaTopic node, got: {:?}",
            nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
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
    fn test_bullmq_new_queue_non_literal_name_records_nothing() {
        let code = r#"
const emailQueue = new Queue(QUEUE_NAME);
"#;
        let (nodes, _) = parse(code);

        // `QUEUE_NAME` is a variable, not the queue's name; used to fabricate
        // a Queue node named after the variable itself.
        assert!(
            !nodes.iter().any(|n| n.kind == NodeKind::Queue),
            "a non-literal queue name must not fabricate a Queue node, got: {:?}",
            nodes.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
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
        // Direct `Produces` and `Consumes` edges carry topic routing via a
        // two-hop walk through the topic node with linear edge complexity.
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

    /// A NestJS `ClientGrpc.getService<XServiceClient>('XService')` call site
    /// is recorded as an RPC call attributed to its enclosing method, allowing
    /// `ContractGraph::reconcile_edges` to build a `CallsRpc` edge.
    #[test]
    fn nestjs_get_service_call_is_recorded_as_an_rpc_call() {
        let code = r#"
import { Injectable, Inject, OnModuleInit } from '@nestjs/common';
import { ClientGrpc } from '@nestjs/microservices';

@Injectable()
export class CheckoutGateway implements OnModuleInit {
    private checkoutService: CheckoutServiceClient;

    constructor(@Inject('CHECKOUT_PACKAGE') private client: ClientGrpc) {}

    onModuleInit() {
        this.checkoutService = this.client.getService<CheckoutServiceClient>('CheckoutService');
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("gateways/rpc/Checkout.gateway.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );

        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "onModuleInit")
            .expect("enclosing method node present");

        assert!(
            rpc_calls
                .iter()
                .any(|(idx, target)| *idx == caller_idx && target.as_str() == "CheckoutService"),
            "expected an rpc_calls entry attributing CheckoutService to \
             onModuleInit, got: {rpc_calls:?}"
        );
    }

    /// Same call site, but with no generic type argument (plain-JS-style
    /// call) — must fall back to the string-literal argument.
    #[test]
    fn nestjs_get_service_call_falls_back_to_string_literal_argument() {
        let code = r#"
export class CheckoutGateway {
    onModuleInit() {
        this.checkoutService = this.client.getService('CheckoutService');
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("gateways/rpc/Checkout.gateway.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );
        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "onModuleInit")
            .expect("enclosing method node present");
        assert!(rpc_calls
            .iter()
            .any(|(idx, target)| *idx == caller_idx && target.as_str() == "CheckoutService"));
    }

    /// Same call site with interface type prefix ICheckoutServiceClient -> CheckoutService.
    #[test]
    fn nestjs_get_service_call_handles_interface_prefix_type_arg() {
        let code = r#"
export class CheckoutGateway {
    onModuleInit() {
        this.checkoutService = this.client.getService<ICheckoutServiceClient>('CheckoutService');
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("gateways/rpc/Checkout.gateway.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );
        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "onModuleInit")
            .expect("enclosing method node present");
        assert!(rpc_calls
            .iter()
            .any(|(idx, target)| *idx == caller_idx && target.as_str() == "CheckoutService"));
    }

    #[test]
    fn nestjs_get_service_call_type_arg_only_with_i_prefix_and_stub_suffix() {
        let code = r#"
export class CheckoutGateway {
    onModuleInit() {
        this.checkoutService = this.client.getService<proto.checkout.ICheckoutServiceStub>();
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("gateways/rpc/Checkout.gateway.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );
        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "onModuleInit")
            .expect("enclosing method node present");
        assert!(rpc_calls
            .iter()
            .any(|(idx, target)| *idx == caller_idx && target.as_str() == "CheckoutService"));
    }

    #[test]
    fn nestjs_get_service_call_prioritizes_literal_over_custom_generic() {
        let code = r#"
export class CheckoutGateway {
    onModuleInit() {
        this.checkoutService = this.client.getService<CustomWrapperType>('RealProtoService');
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("gateways/rpc/Checkout.gateway.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );
        let caller_idx = nodes
            .iter()
            .position(|n| n.name == "onModuleInit")
            .expect("enclosing method node present");
        assert!(rpc_calls
            .iter()
            .any(|(idx, target)| *idx == caller_idx && target.as_str() == "RealProtoService"));
    }

    #[test]
    fn test_ts_import_from_substring_safety() {
        let code = r#"
import { fromEvent } from 'rxjs';
import { escapeFromHtml } from './security';
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let _ = TypeScriptExtractor::extract_with_config(
            Path::new("app.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[],
        );
        let imported_modules: Vec<&str> = imports.iter().map(|(_, m)| m.as_str()).collect();
        assert_eq!(
            imported_modules,
            vec!["rxjs", "fromEvent", "./security", "escapeFromHtml"]
        );
        assert!(
            !imported_modules.iter().any(|m| m.contains("} from")),
            "imported modules must not be corrupted by identifiers containing 'from': {:?}",
            imported_modules
        );
    }

    #[test]
    fn test_ts_rest_delete_patch_endpoints() {
        let code = r#"
export class UserController {
    @Delete(':id')
    deleteUser() {}

    @Patch(':id')
    updateUser() {}
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new("user.controller.ts"),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[],
        );
        let del = nodes
            .iter()
            .find(|n| n.name == "deleteUser")
            .expect("deleteUser");
        assert_eq!(del.kind, NodeKind::HttpEndpoint);
        let patch = nodes
            .iter()
            .find(|n| n.name == "updateUser")
            .expect("updateUser");
        assert_eq!(patch.kind, NodeKind::HttpEndpoint);
    }

    fn extract_rpc(file: &str, code: &str) -> (Vec<ContractNode>, Vec<(usize, CompactStr)>) {
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();
        let mut rpc_calls = Vec::new();
        let tree = parser.parse(code, None).expect("parse");
        let nodes = TypeScriptExtractor::extract_with_config(
            Path::new(file),
            code,
            1,
            &tree,
            &mut imports,
            &mut rpc_calls,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        );
        (nodes, rpc_calls)
    }

    /// Builds a graph declaring `services` (as a `.proto` would) plus one
    /// TS file's nodes and RPC calls, reconciles it, and returns the
    /// `(caller name, service name)` of every `CallsRpc` edge.
    fn rpc_edges_against(services: &[&str], file: &str, code: &str) -> Vec<(String, String)> {
        let (nodes, rpc_calls) = extract_rpc(file, code);
        let mut graph = ContractGraph::new();
        for service in services {
            graph.add_node(ContractNode {
                id: 0,
                name: CompactStr::new(*service),
                kind: NodeKind::GrpcService,
                file_path: Arc::from(Path::new("pb/demo.proto")),
                line_start: 1,
                line_end: 1,
                package: CompactStr::new("oteldemo"),
                repo_id: 0,
                signature: None,
                docstring: None,
            });
        }
        let ids: Vec<_> = nodes.into_iter().map(|n| graph.add_node(n)).collect();
        for (idx, target) in &rpc_calls {
            graph.add_rpc_call(ids[*idx], target);
        }
        graph.reconcile_edges();
        let mut edges: Vec<(String, String)> = graph
            .all_edges()
            .iter()
            .filter(|e| e.kind == EdgeKind::CallsRpc)
            .filter_map(|e| {
                Some((
                    graph.get_node(e.from)?.name.to_string(),
                    graph.get_node(e.to)?.name.to_string(),
                ))
            })
            .collect();
        edges.sort();
        edges
    }

    /// The otel-demo `src/frontend/gateways/rpc/Ad.gateway.ts` shape: a
    /// ts-proto client imported from the generated file, built once at
    /// module level, used from an object-literal method.
    const AD_GATEWAY: &str = r#"
import { ChannelCredentials } from '@grpc/grpc-js';
import { AdResponse, AdServiceClient } from '../../protos/demo';

const { AD_ADDR = '' } = process.env;

const client = new AdServiceClient(AD_ADDR, ChannelCredentials.createInsecure());

const AdGateway = () => ({
  listAds(contextKeys: string[]) {
    return new Promise<AdResponse>((resolve, reject) =>
      client.getAds({ contextKeys: contextKeys }, (error, response) => (error ? reject(error) : resolve(response)))
    );
  },
});

export default AdGateway();
"#;

    /// Declares `services` (as a `.proto` would), indexes each `(path, code)`
    /// TS file through the real per-file pipeline (`PolyglotIndexer`, so the
    /// `Imports` attribution runs too) and reconciles.
    fn index_against(services: &[&str], files: &[(&str, &str)]) -> ContractGraph {
        let mut graph = ContractGraph::new();
        for service in services {
            graph.add_node(ContractNode {
                id: 0,
                name: CompactStr::new(*service),
                kind: NodeKind::GrpcService,
                file_path: Arc::from(Path::new("pb/demo.proto")),
                line_start: 1,
                line_end: 1,
                package: CompactStr::new("oteldemo"),
                repo_id: 0,
                signature: None,
                docstring: None,
            });
        }
        for (file, code) in files {
            crate::languages::PolyglotIndexer::index_file(Path::new(file), code, 1, &mut graph);
        }
        graph.reconcile_edges();
        graph
    }

    /// Plan 4 step 4.5 — a module-level construction resolving to a declared
    /// service is attributed to the declaration that uses the client
    /// (`listAds`), not to a node synthesized for the binding: the file's
    /// node set is exactly its declarations.
    #[test]
    fn imported_ts_proto_client_construction_links_to_declared_service() {
        let file = "src/frontend/gateways/rpc/Ad.gateway.ts";
        let (nodes, rpc_calls) = extract_rpc(file, AD_GATEWAY);
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["listAds"], "no synthetic node for `client`");
        assert_eq!(rpc_calls, vec![(0, CompactStr::new("AdService"))]);

        assert_eq!(
            rpc_edges_against(&["AdService", "CartService"], file, AD_GATEWAY),
            vec![("listAds".to_string(), "AdService".to_string())]
        );
    }

    /// Review finding (PR #39): a module-level `new XClient()` that resolves
    /// to no declared service — an SDK client, react-query's `QueryClient` —
    /// leaves no trace at all: no node, no `CallsRpc`, no `Imports` edge.
    #[test]
    fn module_level_client_without_declared_service_leaves_no_trace() {
        let code = r#"
import { S3Client } from '@aws-sdk/client-s3';
import { QueryClient } from '@tanstack/react-query';

const s3 = new S3Client({});
const queryClient = new QueryClient();
"#;
        let graph = index_against(&["AdService"], &[("src/frontend/pages/_app.tsx", code)]);
        let names: Vec<&str> = graph.all_nodes().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["AdService"]);
        assert_eq!(graph.edge_count(), 0, "{:?}", graph.all_edges());

        // Used from a declaration, the client adds no node or edge either:
        // the graph is the one the file yields without the construction. Its
        // pending `rpc_call` (`put -> "S3"`) is the only difference — the
        // same unresolved state any `getService<...>` call to an undeclared
        // service leaves, re-resolved on every `reconcile_edges` should a
        // matching service ever be declared.
        let used = r#"
import { S3Client } from '@aws-sdk/client-s3';

const s3 = new S3Client({});

export const Store = () => ({
  put(key: string) {
    return s3.send(key);
  },
});
"#;
        let without = used.replace("const s3 = new S3Client({});", "");
        let lines = |code: &str| {
            let mut lines =
                index_against(&["AdService"], &[("src/web/store.ts", code)]).canonical_lines();
            lines.retain(|l| !l.starts_with("rpc_call "));
            lines
        };
        assert_eq!(lines(used), lines(&without));
    }

    /// Several otel-demo gateways each bind `const client = new
    /// <X>ServiceClient(...)`: every file's call lands on its own, real
    /// declaration (distinct file, distinct name) — never on N homonymous
    /// `client` nodes — and the result does not depend on indexing order.
    #[test]
    fn same_binding_name_in_two_files_yields_distinct_stable_callers() {
        let cart = AD_GATEWAY
            .replace("AdServiceClient", "CartServiceClient")
            .replace("AdResponse", "Cart")
            .replace("listAds", "getCart")
            .replace("getAds", "getCart");
        let ad_file = "src/frontend/gateways/rpc/Ad.gateway.ts";
        let cart_file = "src/frontend/gateways/rpc/Cart.gateway.ts";
        let services = ["AdService", "CartService"];

        let graph = index_against(&services, &[(ad_file, AD_GATEWAY), (cart_file, &cart)]);
        let mut edges: Vec<(String, String, String)> = graph
            .all_edges()
            .iter()
            .filter(|e| e.kind == EdgeKind::CallsRpc)
            .filter_map(|e| {
                let from = graph.get_node(e.from)?;
                Some((
                    from.file_path.to_string_lossy().into_owned(),
                    from.name.to_string(),
                    graph.get_node(e.to)?.name.to_string(),
                ))
            })
            .collect();
        edges.sort();
        assert_eq!(
            edges,
            vec![
                (ad_file.into(), "listAds".into(), "AdService".into()),
                (cart_file.into(), "getCart".into(), "CartService".into()),
            ]
        );
        assert!(!graph.all_nodes().any(|n| n.name == "client"));

        let reversed = index_against(&services, &[(cart_file, &cart), (ad_file, AD_GATEWAY)]);
        assert_eq!(graph.canonical_lines(), reversed.canonical_lines());
    }

    /// Every declaration referencing the binding (plain or shorthand) is a
    /// caller, once; a property named like it (`this.client`) is not a
    /// reference; a construction nothing declared references is dropped
    /// rather than given a node of its own.
    #[test]
    fn module_level_client_attributed_to_each_referencing_declaration() {
        let code = r#"
import { CartServiceClient, AdServiceClient } from './gen/demo';

const client = new CartServiceClient(ADDR);
const unused = new AdServiceClient(ADDR);

export class CartStore {
    add(item) {
        client.addItem(item);
        return client.getCart();
    }
    deps() {
        return { client };
    }
    other() {
        return this.client;
    }
}
"#;
        assert_eq!(
            rpc_edges_against(&["AdService", "CartService"], "src/web/cart.ts", code),
            vec![
                ("add".to_string(), "CartService".to_string()),
                ("deps".to_string(), "CartService".to_string()),
            ]
        );
    }

    /// Inside a method, the construction is attributed to that method (no
    /// synthetic node); aliased and namespace imports pass the import gate;
    /// `<X>Client` for a declared `<X>Service` resolves too.
    #[test]
    fn client_construction_in_method_aliased_and_namespace_imports() {
        let code = r#"
import { CartServiceClient as Carts } from './gen/demo';
import * as demo from './gen/demo';
import { AdClient } from './gen/ad';

export class Gateway {
    connect() {
        this.carts = new Carts(ADDR, creds);
        this.checkout = new demo.CheckoutServiceClient(ADDR, creds);
        this.ads = new AdClient(ADDR, creds);
    }
}
"#;
        let (nodes, _) = extract_rpc("gw.ts", code);
        assert!(
            !nodes.iter().any(|n| n
                .signature
                .as_deref()
                .is_some_and(|s| s.starts_with("new "))),
            "an enclosed construction must not synthesize a caller node"
        );
        assert_eq!(
            rpc_edges_against(
                &["AdService", "CartService", "CheckoutService"],
                "gw.ts",
                code
            ),
            vec![
                ("connect".to_string(), "AdService".to_string()),
                ("connect".to_string(), "CartService".to_string()),
                ("connect".to_string(), "CheckoutService".to_string()),
            ]
        );
    }

    /// An imported `new FooClient()` whose service is not declared in the
    /// graph (an SDK client, a react-query `QueryClient`, ...) is no edge:
    /// the declared services, not the import path, decide.
    #[test]
    fn imported_client_without_declared_service_is_not_an_edge() {
        let code = r#"
import { S3Client } from '@aws-sdk/client-s3';
import { FooClient } from '../../protos/foo';

const s3 = new S3Client({});
const foo = new FooClient(ADDR);
"#;
        assert!(rpc_edges_against(&["AdService"], "src/frontend/x.ts", code).is_empty());
    }

    /// A `<X>Client` that is not imported (declared locally, or a global) is
    /// never recorded, even when `<X>` is a declared service.
    #[test]
    fn non_imported_client_construction_is_not_recorded() {
        let code = r#"
class AdServiceClient {}
const client = new AdServiceClient(ADDR);
const other = new CartServiceClient(ADDR);
"#;
        let (_, rpc_calls) = extract_rpc("src/frontend/x.ts", code);
        assert!(rpc_calls.is_empty(), "got: {rpc_calls:?}");
        assert!(
            rpc_edges_against(&["AdService", "CartService"], "src/frontend/x.ts", code).is_empty()
        );
    }

    /// No import-path heuristic: an import from `./prototype` (which a
    /// `proto` substring test would have matched) links nothing unless the
    /// client names a declared service — and the path neither helps nor
    /// hurts when it does.
    #[test]
    fn import_path_plays_no_role_in_client_detection() {
        let code = r#"
import { PrototypeClient } from './prototype';

const proto = new PrototypeClient();
"#;
        assert!(rpc_edges_against(&["AdService"], "src/web/p.ts", code).is_empty());

        let code = r#"
import { AdServiceClient } from './prototype';

const ads = new AdServiceClient(ADDR);

export const Ads = () => ({
  list() {
    return ads.getAds();
  },
});
"#;
        assert_eq!(
            rpc_edges_against(&["AdService"], "src/web/p.ts", code),
            vec![("list".to_string(), "AdService".to_string())]
        );
    }
}
