use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

/// `(nodes, dependencies, producers, rpc_calls)`, the latter three keyed by
/// local node index in the same shape `languages::FileIndex.dependencies` /
/// `.producers` / `.rpc_calls` expect.
type ExtractedRelations = (
    Vec<ContractNode>,
    Vec<(usize, CompactStr)>,
    Vec<(usize, CompactStr)>,
    Vec<(usize, CompactStr)>,
);

pub struct JavaExtractor;

impl JavaExtractor {
    /// Test/ad-hoc entry point: parses `content` itself. Production indexing goes
    /// through [`Self::extract_relations`] via `PolyglotIndexer`, which parses once
    /// with `AstGuard::parse_with` so a parse failure is visible instead of silently
    /// producing an empty result indistinguishable from a legitimately empty file.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        let Some(tree) = parser.parse(content, None) else {
            return Vec::new();
        };
        let (nodes, _dependencies, _producers, _rpc_calls) =
            Self::extract_relations(file_path, content, repo_id, &tree);
        nodes
    }

    /// Extended extraction: returns the declared nodes plus two relations keyed by
    /// *local* node index, in the same shape `languages::FileIndex.dependencies` /
    /// `.producers` expect (see the TypeScript branch in `languages::mod` for the
    /// established pattern):
    ///   - `dependencies`: `import` targets (plain, `static`, and wildcard) resolved
    ///     against the node whose line range actually references the imported
    ///     symbol, mirroring the TS "is it used in this node's body" heuristic so a
    ///     file-level import doesn't fan out to every symbol in the file.
    ///   - `producers`: `KafkaTemplate.send(...)` call sites found within a method
    ///     body, keyed to that method's node.
    pub fn extract_relations(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> ExtractedRelations {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let mut producers = Vec::new();
        let mut rpc_calls = Vec::new();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = CompactStr::default();

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
            &mut producers,
            &mut rpc_calls,
            None,
            0,
        );

        let imports = Self::collect_imports(root, source_bytes);
        let mut dependencies = Vec::new();
        if !imports.is_empty() {
            let content_lines: Vec<&str> = content.lines().collect();
            for (i, node) in nodes.iter().enumerate() {
                let body = content_lines
                    .get(node.line_start.saturating_sub(1)..node.line_end)
                    .unwrap_or(&[]);
                for (check_symbol, target, always_attach) in &imports {
                    let is_used =
                        *always_attach || body.iter().any(|l| l.contains(check_symbol.as_str()));
                    if is_used {
                        dependencies.push((i, CompactStr::new(target.as_str())));
                    }
                }
            }
        }

        (nodes, dependencies, producers, rpc_calls)
    }

    /// Walks the tree collecting `import_declaration` nodes into
    /// `(check_symbol, dependency_target, always_attach)` triples:
    ///   - plain `import a.b.C;`      -> check "C",  target "a.b.C",   attach on use
    ///   - static `import a.b.C.M;`  -> check "M",  target "a.b.C",   attach on use
    ///   - wildcard `import a.b.*;`  -> check "",   target "a.b",     always attach
    fn collect_imports(node: Node, source: &[u8]) -> Vec<(String, String, bool)> {
        let mut imports = Vec::new();
        Self::collect_imports_inner(node, source, &mut imports, 0);
        imports
    }

    fn collect_imports_inner(
        node: Node,
        source: &[u8],
        out: &mut Vec<(String, String, bool)>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "import_declaration" {
            if let Ok(text) = node.utf8_text(source) {
                let mut clean = text
                    .trim_start_matches("import")
                    .trim()
                    .trim_end_matches(';')
                    .trim();
                let is_static = clean.starts_with("static");
                if is_static {
                    clean = clean.trim_start_matches("static").trim();
                }

                if let Some(pkg) = clean.strip_suffix(".*") {
                    out.push((String::new(), pkg.to_string(), true));
                } else if is_static {
                    // `a.b.C.member` -> dependency on the declaring class `a.b.C`,
                    // usage-checked against the bare `member` identifier.
                    if let Some((class_path, member)) = clean.rsplit_once('.') {
                        out.push((member.to_string(), class_path.to_string(), false));
                    }
                } else if let Some((_, symbol)) = clean.rsplit_once('.') {
                    out.push((symbol.to_string(), clean.to_string(), false));
                } else if !clean.is_empty() {
                    out.push((clean.to_string(), clean.to_string(), false));
                }
            }
            // Imports never nest further declarations of interest.
            return;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_imports_inner(child, source, out, depth + 1);
        }
    }

    /// grpc-java's own universal codegen convention (protoc-gen-grpc-java,
    /// not Spring-specific): a client stub is built as
    /// `<Service>Grpc.newBlockingStub(channel)` / `.newStub(...)` /
    /// `.newFutureStub(...)`. Returns `<Service>` when found — e.g.
    /// `AdServiceGrpc.newBlockingStub(channel)` (Online Boutique's real
    /// adservice client) yields `"AdService"`.
    /// Finds every `<Service>Grpc.new{Blocking,Future,}Stub(...)` call in
    /// `text`, in source order. Returns all of them, not just the first —
    /// a constructor building stubs for two different services (`var a =
    /// FooServiceGrpc.newStub(ch); var b = BarServiceGrpc.newBlockingStub(ch);`)
    /// must not lose the earlier one just because a *different* suffix
    /// happens to be checked first in a fixed candidate list.
    fn extract_grpc_client_services(text: &str) -> Vec<String> {
        let mut hits: Vec<(usize, &str)> = Vec::new();
        for suffix in [".newBlockingStub(", ".newStub(", ".newFutureStub("] {
            let mut search_start = 0;
            while let Some(rel_idx) = text[search_start..].find(suffix) {
                let idx = search_start + rel_idx;
                hits.push((idx, suffix));
                search_start = idx + suffix.len();
            }
        }
        hits.sort_by_key(|(idx, _)| *idx);

        let mut services = Vec::new();
        for (idx, _) in hits {
            let before = &text[..idx];
            let ident_start = before
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map(|p| p + 1)
                .unwrap_or(0);
            let ident = &before[ident_start..];
            if let Some(service) = ident.strip_suffix("Grpc") {
                if !service.is_empty() {
                    services.push(service.to_string());
                }
            }
        }
        services
    }

    /// Finds `<kafkaIdent>.send(<topic>, ...)` call sites within `text`, returning
    /// the first topic literal found (Spring's `KafkaTemplate.send` producer API).
    /// Finds `<kafkaIdent>.send(<topic>, ...)` and returns `<topic>` only
    /// when it's a genuine string literal in first-argument position. The
    /// search used to look for the first `"` anywhere in the *rest of the
    /// method text* after `.send(` with no bound on the call's own closing
    /// paren — so `kafkaTemplate.send(topicVar, msg); logger.info("...")`
    /// could walk straight past the call and pick up an unrelated string
    /// literal from a completely different statement as the "topic".
    fn extract_kafka_producer_topic(text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        let mut search_start = 0;
        while let Some(rel_idx) = lower[search_start..].find(".send(") {
            let idx = search_start + rel_idx;
            let before = &text[..idx];
            let ident_start = before
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map(|p| p + 1)
                .unwrap_or(0);
            let receiver = &before[ident_start..idx];
            let call_start = idx + ".send(".len();
            if receiver.to_lowercase().contains("kafka") {
                // Bound the search to this call's own argument list by
                // matching balanced parens from the '(' already consumed.
                let mut depth = 1i32;
                let mut call_end = None;
                for (i, c) in text[call_start..].char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                call_end = Some(call_start + i);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(call_end) = call_end {
                    let args = text[call_start..call_end].trim_start();
                    // Only a literal in first-argument position counts —
                    // not any quote found later among the other arguments.
                    if let Some(rest) = args.strip_prefix('"') {
                        if let Some(q_end) = rest.find('"') {
                            return Some(rest[..q_end].to_string());
                        }
                    }
                }
            }
            search_start = call_start;
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
        producers: &mut Vec<(usize, CompactStr)>,
        rpc_calls: &mut Vec<(usize, CompactStr)>,
        enclosing_class_idx: Option<usize>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        let mut child_enclosing_class_idx = enclosing_class_idx;

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
                // Plain (non-Spring) grpc-java server: `class FooServiceImpl
                // extends FooServiceGrpc.FooServiceImplBase`. The generated
                // `<Service>Grpc.<Service>ImplBase` base class is grpc-java's
                // own universal convention (protoc-gen-grpc-java), unlike
                // `@GrpcService`, which is Spring-specific and already
                // covered above. Requires both "Grpc" and "ImplBase" in the
                // superclass reference, not "ImplBase" alone — a bare
                // substring match would also tag any unrelated
                // `*ImplBase`-suffixed base class from a non-gRPC framework
                // using the same generic naming convention.
                if node
                    .child_by_field_name("superclass")
                    .and_then(|s| s.utf8_text(source).ok())
                    .is_some_and(|t| t.contains("Grpc") && t.contains("ImplBase"))
                {
                    kind = NodeKind::GrpcService;
                }

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(class_name),
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(format!("class {class_name}"))),
                    docstring: None,
                });
                child_enclosing_class_idx = Some(nodes.len() - 1);
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

                if Self::has_annotation(&full_anno, "KafkaListener") {
                    kind = NodeKind::KafkaTopic;
                    topic_target = Self::extract_annotation_text(&full_anno, "KafkaListener")
                        .and_then(|scoped| Self::extract_annotation_param(scoped, "topics"));
                } else if Self::has_annotation(&full_anno, "GetMapping")
                    || Self::has_annotation(&full_anno, "PostMapping")
                    || Self::has_annotation(&full_anno, "PutMapping")
                    || Self::has_annotation(&full_anno, "DeleteMapping")
                    || Self::has_annotation(&full_anno, "PatchMapping")
                    || Self::has_annotation(&full_anno, "RequestMapping")
                    || Self::has_annotation(&full_anno, "GET")
                    || Self::has_annotation(&full_anno, "POST")
                    || Self::has_annotation(&full_anno, "PUT")
                    || Self::has_annotation(&full_anno, "DELETE")
                    || Self::has_annotation(&full_anno, "PATCH")
                    || Self::has_annotation(&full_anno, "Path")
                    || Self::has_annotation(&full_anno, "Get")
                    || Self::has_annotation(&full_anno, "Post")
                    || Self::has_annotation(&full_anno, "Put")
                    || Self::has_annotation(&full_anno, "Delete")
                    || Self::has_annotation(&full_anno, "Patch")
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

                let method_text = node.utf8_text(source).unwrap_or("");
                let producer_topic = Self::extract_kafka_producer_topic(method_text);
                let grpc_client_services = Self::extract_grpc_client_services(method_text);

                nodes.push(ContractNode {
                    id: 0,
                    name: final_name,
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: sig_str,
                    docstring: None,
                });

                if let Some(topic) = producer_topic {
                    producers.push((nodes.len() - 1, CompactStr::new(topic)));
                }
                for service in grpc_client_services {
                    rpc_calls.push((nodes.len() - 1, CompactStr::new(service)));
                }
            }
            "constructor_declaration" => {
                // grpc-java's client stub is conventionally built in the
                // client class's own constructor (`AdServiceClient(ManagedChannel
                // channel) { blockingStub = AdServiceGrpc.newBlockingStub(channel); }`
                // — Online Boutique's real `AdServiceClient.java` does exactly
                // this), not necessarily a named method. Constructors aren't
                // otherwise extracted as their own `ContractNode`s, so any
                // stub built here is attributed to the *enclosing class's*
                // node instead of a synthetic constructor node.
                if let Some(class_idx) = enclosing_class_idx {
                    let ctor_text = node.utf8_text(source).unwrap_or("");
                    for service in Self::extract_grpc_client_services(ctor_text) {
                        rpc_calls.push((class_idx, CompactStr::new(service)));
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
                producers,
                rpc_calls,
                child_enclosing_class_idx,
                depth + 1,
            );
        }
    }

    fn has_annotation(full_anno: &str, name: &str) -> bool {
        let needle = format!("@{name}");
        let mut start = 0;
        while let Some(pos) = full_anno[start..].find(&needle) {
            let actual_pos = start + pos;
            let after_idx = actual_pos + needle.len();
            let prev_ok = actual_pos == 0
                || full_anno[..actual_pos]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let next_ok = after_idx >= full_anno.len()
                || full_anno[after_idx..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if prev_ok && next_ok {
                return true;
            }
            start = actual_pos + 1;
        }
        false
    }

    /// Isolates just `@name(...)`'s own parenthesized text out of `full_anno`
    /// — which may hold several concatenated annotations on the same method
    /// (`@Transactional("x") @KafkaListener(groupId = "g1")`) — by matching
    /// balanced parens starting at `@name`'s own `(`. Without this,
    /// `extract_annotation_param`'s fallback used to search from the *first*
    /// `(` in the whole blob to the *last* `)`, which can span straight
    /// through an unrelated annotation and pick up its argument instead.
    fn extract_annotation_text<'a>(full_anno: &'a str, name: &str) -> Option<&'a str> {
        let needle = format!("@{name}");
        let at = full_anno.find(&needle)?;
        let rest = &full_anno[at + needle.len()..];
        let open_rel = rest.find('(')?;
        // Must be only whitespace between the annotation name and '(' —
        // otherwise this "@name" match is a prefix of a longer identifier
        // that `has_annotation`'s stricter boundary check would reject.
        if !rest[..open_rel].chars().all(char::is_whitespace) {
            return None;
        }
        let mut depth = 0usize;
        for (i, c) in rest.char_indices().skip(open_rel) {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&rest[open_rel..=i]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn extract_annotation_param(annotation: &str, param: &str) -> Option<String> {
        if let Some(idx) = annotation.find(param) {
            let rest = annotation[idx + param.len()..].trim_start();
            let after_eq = if let Some(stripped) = rest.strip_prefix('=') {
                stripped.trim_start()
            } else {
                rest
            };
            if let Some(quote_start) = after_eq.find('"') {
                if let Some(quote_end) = after_eq[quote_start + 1..].find('"') {
                    let val = &after_eq[quote_start + 1..quote_start + 1 + quote_end];
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
        // Fallback: a direct positional string literal, e.g.
        // `@KafkaListener("orders.created")` — with no named parameter at
        // all. `annotation` must already be scoped to just this one
        // annotation's own parens (see `extract_annotation_text`). If the
        // body instead holds a named param unrelated to `param` (e.g.
        // `@KafkaListener(groupId = "billing-group")`, and `param` is
        // "topics"), that value is not a topic and must not be picked up
        // just because it's the only quoted string present.
        if let Some(p_start) = annotation.find('(') {
            if let Some(p_end) = annotation.rfind(')') {
                let inside = &annotation[p_start + 1..p_end];
                if inside.contains('=') {
                    return None;
                }
                if let Some(q_start) = inside.find('"') {
                    if let Some(q_end) = inside[q_start + 1..].find('"') {
                        let val = &inside[q_start + 1..q_start + 1 + q_end];
                        if !val.is_empty() {
                            return Some(val.to_string());
                        }
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{ContractGraph, NodeId};

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

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

    /// Plain (non-Spring) grpc-java server: a class extending the generated
    /// `<Service>Grpc.<Service>ImplBase` (protoc-gen-grpc-java's own
    /// universal convention, e.g. Online Boutique's real
    /// `AdServiceImpl extends AdServiceGrpc.AdServiceImplBase`) must be
    /// tagged `GrpcService` — previously only Spring's `@GrpcService`
    /// annotation was recognized.
    #[test]
    fn grpc_java_impl_base_subclass_is_tagged_grpc_service() {
        let code = r#"
package hipstershop;

class AdServiceImpl extends AdServiceGrpc.AdServiceImplBase {
    public void getAds(AdRequest req, StreamObserver<AdResponse> responseObserver) {}
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let nodes = JavaExtractor::extract(Path::new("AdService.java"), code, 1, &mut parser);
        let node = nodes
            .iter()
            .find(|n| n.name == "AdServiceImpl")
            .expect("AdServiceImpl node");
        assert_eq!(node.kind, NodeKind::GrpcService);
    }

    /// grpc-java's client stub is built as `<Service>Grpc.newBlockingStub(channel)`
    /// / `.newStub(...)` / `.newFutureStub(...)` — the real shape in Online
    /// Boutique's own `AdServiceClient.java`
    /// (`blockingStub = hipstershop.AdServiceGrpc.newBlockingStub(channel);`).
    /// Must be recorded as an RPC call to `AdService`.
    #[test]
    fn grpc_java_client_stub_construction_is_recorded_as_an_rpc_call() {
        let code = r#"
package hipstershop;

class AdServiceClient {
    private void createStub() {
        blockingStub = hipstershop.AdServiceGrpc.newBlockingStub(channel);
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _deps, _producers, rpc_calls) =
            JavaExtractor::extract_relations(Path::new("AdServiceClient.java"), code, 1, &tree);
        assert_eq!(
            rpc_calls.len(),
            1,
            "expected 1 rpc call, got: {:?}",
            rpc_calls
        );
        assert_eq!(rpc_calls[0].1.as_str(), "AdService");
        assert_eq!(nodes[rpc_calls[0].0].name.as_str(), "createStub");
    }

    /// The real, motivating shape (Online Boutique's actual
    /// `AdServiceClient.java`): the stub is built in the class's own
    /// *constructor*, not a named method. Constructors aren't extracted as
    /// their own `ContractNode`s, so the rpc_call must be attributed to the
    /// enclosing class instead.
    #[test]
    fn grpc_java_client_stub_construction_in_constructor_is_recorded() {
        let code = r#"
package hipstershop;

class AdServiceClient {
    private AdServiceClient(ManagedChannel channel) {
        blockingStub = hipstershop.AdServiceGrpc.newBlockingStub(channel);
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _deps, _producers, rpc_calls) =
            JavaExtractor::extract_relations(Path::new("AdServiceClient.java"), code, 1, &tree);
        assert_eq!(
            rpc_calls.len(),
            1,
            "expected 1 rpc call from the constructor, got: {:?}",
            rpc_calls
        );
        assert_eq!(rpc_calls[0].1.as_str(), "AdService");
        assert_eq!(nodes[rpc_calls[0].0].name.as_str(), "AdServiceClient");
    }

    /// Two stubs for two different services built in the same method must
    /// both be recorded, in source order — a fixed candidate-suffix list
    /// checked in a fixed order must not silently drop whichever suffix
    /// isn't checked first.
    #[test]
    fn grpc_java_multiple_stub_constructions_in_one_method_are_all_recorded() {
        let code = r#"
package hipstershop;

class MultiClient {
    private void createStubs() {
        var a = FooServiceGrpc.newStub(channel);
        var b = BarServiceGrpc.newBlockingStub(channel);
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(code, None).expect("parse");
        let (_nodes, _deps, _producers, rpc_calls) =
            JavaExtractor::extract_relations(Path::new("MultiClient.java"), code, 1, &tree);
        let services: Vec<&str> = rpc_calls.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            services,
            vec!["FooService", "BarService"],
            "both stubs must be recorded, in source order"
        );
    }

    /// A class extending an unrelated `*ImplBase` base class from a
    /// non-gRPC framework using the same generic naming convention must not
    /// be mistagged `GrpcService` — the superclass reference must contain
    /// both "Grpc" and "ImplBase", the real grpc-java shape, not "ImplBase"
    /// alone.
    #[test]
    fn unrelated_impl_base_subclass_is_not_tagged_grpc_service() {
        let code = r#"
class Foo extends SomeCorbaSkeletonImplBase {
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let nodes = JavaExtractor::extract(Path::new("Foo.java"), code, 1, &mut parser);
        let node = nodes.iter().find(|n| n.name == "Foo").expect("Foo node");
        assert_ne!(
            node.kind,
            NodeKind::GrpcService,
            "an unrelated *ImplBase convention must not be mistaken for grpc-java's own"
        );
    }

    /// End-to-end: a Java client's stub construction must actually resolve
    /// to a real `CallsRpc` edge through `ContractGraph::reconcile_edges`
    /// when a `.proto`-declared `GrpcService` node exists for the target —
    /// the real shape every golden-corpus repo uses (a service always has a
    /// `.proto` declaration; nothing here depends on the impl class's own
    /// name, which conventionally differs from the bare service name, e.g.
    /// `AdServiceImpl` vs. `AdService` — deliberately not "fixed" by loosening
    /// `ContractGraph`'s own bare-name matching, since a generic `Impl`
    /// suffix is far too common outside gRPC to safely strip there).
    #[test]
    fn grpc_java_client_stub_resolves_to_a_real_edge_against_a_proto_declared_service() {
        let mut graph = ContractGraph::new();

        let service_id = graph.add_node(ContractNode {
            id: 0,
            name: CompactStr::new("AdService"),
            kind: NodeKind::GrpcService,
            file_path: Path::new("protos/demo.proto").into(),
            line_start: 1,
            line_end: 1,
            package: CompactStr::new("hipstershop"),
            repo_id: 0,
            signature: Some(CompactStr::new("service AdService")),
            docstring: None,
        });

        let code = r#"
package hipstershop;

class AdServiceClient {
    private AdServiceClient(ManagedChannel channel) {
        blockingStub = hipstershop.AdServiceGrpc.newBlockingStub(channel);
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_java::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(code, None).expect("parse");
        let (client_nodes, _deps, _producers, rpc_calls) =
            JavaExtractor::extract_relations(Path::new("AdServiceClient.java"), code, 1, &tree);
        let client_ids: Vec<NodeId> = client_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (idx, target) in rpc_calls {
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
            "expected a CallsRpc edge from the AdServiceClient caller to the \
             proto-declared AdService node, found edges: {:?}",
            graph.all_edges()
        );
    }

    /// `@KafkaListener`'s topic fallback must never reach across into a
    /// *different* annotation on the same method. It used to search from the
    /// first `(` to the last `)` across the whole concatenated annotation
    /// blob, so a preceding `@Transactional("payments-tx")` (unrelated to
    /// Kafka) could be picked up as the topic when `@KafkaListener` itself
    /// carries no bare string literal — only a named, non-topic param.
    #[test]
    fn kafka_listener_fallback_does_not_cross_into_unrelated_annotation() {
        let code = r#"
package com.mesh.billing;

public class BillingListener {
    @Transactional("payments-tx")
    @KafkaListener(groupId = "billing-group")
    public void onEvent(String msg) {}
}
"#;
        let mut parser = parser();
        let nodes = JavaExtractor::extract(Path::new("BillingListener.java"), code, 0, &mut parser);
        let node = nodes
            .iter()
            .find(|n| n.kind == NodeKind::KafkaTopic)
            .expect("kafka topic node");
        assert_eq!(
            node.name.as_str(),
            "onEvent",
            "with no resolvable `topics` value, must fall back to the method name — \
             never an unrelated annotation's argument like \"payments-tx\" or the \
             groupId \"billing-group\""
        );
    }

    /// `extract_kafka_producer_topic` must only accept a string literal in
    /// the `.send(...)` call's own first-argument position, bounded by that
    /// call's own closing paren. It used to search for the first `"` in the
    /// *rest of the method text* with no such bound, so a variable-held
    /// topic followed by an unrelated logging statement could have that
    /// statement's string picked up as the "topic" instead.
    #[test]
    fn kafka_producer_topic_does_not_cross_into_later_statement() {
        let code = r#"kafkaTemplate.send(topicVar, payload); logger.info("database is down");"#;
        assert_eq!(JavaExtractor::extract_kafka_producer_topic(code), None);
    }

    #[test]
    fn kafka_producer_topic_extracts_literal_first_argument() {
        let code = r#"kafkaTemplate.send("orders.created", payload);"#;
        assert_eq!(
            JavaExtractor::extract_kafka_producer_topic(code),
            Some("orders.created".to_string())
        );
    }

    // --- Item 2: Java import extraction -----------------------------------

    #[test]
    fn test_java_plain_import_dependency() {
        let code = r#"
package com.mesh.orders;

import com.mesh.billing.BillingService;

public class OrderController {
    public void charge() {
        BillingService svc = new BillingService();
    }
}
"#;
        let mut parser = parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, dependencies, _producers, _rpc_calls) =
            JavaExtractor::extract_relations(Path::new("OrderController.java"), code, 1, &tree);

        let class_idx = nodes
            .iter()
            .position(|n| n.name == "OrderController")
            .expect("class node present");
        assert!(dependencies.iter().any(|(i, target)| {
            *i == class_idx && target.as_str() == "com.mesh.billing.BillingService"
        }));
    }

    #[test]
    fn test_java_static_and_wildcard_imports() {
        let code = r#"
package com.mesh.orders;

import static com.mesh.billing.BillingService.DEFAULT_CURRENCY;
import com.mesh.notifications.*;

public class OrderController {
    public void charge() {
        String currency = DEFAULT_CURRENCY;
    }
}
"#;
        let mut parser = parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, dependencies, _producers, _rpc_calls) =
            JavaExtractor::extract_relations(Path::new("OrderController.java"), code, 1, &tree);

        let class_idx = nodes
            .iter()
            .position(|n| n.name == "OrderController")
            .expect("class node present");

        // static import resolves to the declaring class, used-checked against the
        // bare member identifier (`DEFAULT_CURRENCY`, actually referenced below).
        assert!(dependencies.iter().any(|(i, target)| {
            *i == class_idx && target.as_str() == "com.mesh.billing.BillingService"
        }));
        // wildcard import always attaches (no specific symbol to usage-check).
        assert!(dependencies
            .iter()
            .any(|(i, target)| { *i == class_idx && target.as_str() == "com.mesh.notifications" }));
    }

    #[test]
    fn test_java_unused_import_not_attached_to_unrelated_symbol() {
        let code = r#"
package com.mesh.orders;

import com.mesh.billing.BillingService;

public class OrderController {
    public void ping() {
        System.out.println("pong");
    }
}
"#;
        let mut parser = parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, dependencies, _producers, _rpc_calls) =
            JavaExtractor::extract_relations(Path::new("OrderController.java"), code, 1, &tree);

        let method_idx = nodes
            .iter()
            .position(|n| n.name == "ping")
            .expect("method node present");
        assert!(!dependencies.iter().any(|(i, target)| {
            *i == method_idx && target.as_str() == "com.mesh.billing.BillingService"
        }));
    }

    #[test]
    fn test_java_import_find_dependents_integration() {
        let mut p = parser();

        let billing_code = r#"
package com.mesh.billing;

public class BillingService {
    public void charge() {}
}
"#;
        let order_code = r#"
package com.mesh.orders;

import com.mesh.billing.BillingService;

public class OrderController {
    private BillingService svc;

    public void checkout() {
        svc = new BillingService();
    }
}
"#;

        let mut graph = ContractGraph::new();

        let billing_tree = p.parse(billing_code, None).expect("parse");
        let (billing_nodes, _deps, _producers, _rpc_calls) = JavaExtractor::extract_relations(
            Path::new("services/billing/BillingService.java"),
            billing_code,
            1,
            &billing_tree,
        );
        for node in billing_nodes {
            graph.add_node(node);
        }

        let order_tree = p.parse(order_code, None).expect("parse");
        let (order_nodes, order_deps, _producers2, _rpc_calls2) = JavaExtractor::extract_relations(
            Path::new("services/orders/OrderController.java"),
            order_code,
            2,
            &order_tree,
        );
        let order_ids: Vec<_> = order_nodes.into_iter().map(|n| graph.add_node(n)).collect();
        for (i, target) in order_deps {
            graph.add_dependency(order_ids[i], target.as_str());
        }

        let dependents = graph.find_dependents("com.mesh.billing.BillingService");
        assert!(dependents.iter().any(|n| n.name == "OrderController"));
    }

    // --- Item 3: KafkaTemplate.send producer detection ---------------------

    #[test]
    fn test_java_kafka_producer_extraction() {
        let code = r#"
package com.mesh.orders;

public class OrderService {
    public void placeOrder() {
        kafkaTemplate.send("orders.created", "payload");
    }
}
"#;
        let mut parser = parser();
        let tree = parser.parse(code, None).expect("parse");
        let (nodes, _deps, producers, _rpc_calls) =
            JavaExtractor::extract_relations(Path::new("OrderService.java"), code, 1, &tree);

        let method_idx = nodes
            .iter()
            .position(|n| n.name == "placeOrder")
            .expect("method node present");
        assert!(producers
            .iter()
            .any(|(i, topic)| { *i == method_idx && topic.as_str() == "orders.created" }));
    }

    #[test]
    fn test_java_non_kafka_send_call_is_not_a_producer() {
        let code = r#"
package com.mesh.orders;

public class OrderService {
    public void notifyUser() {
        emailClient.send("welcome@example.com", "payload");
    }
}
"#;
        let mut parser = parser();
        let tree = parser.parse(code, None).expect("parse");
        let (_nodes, _deps, producers, _rpc_calls) =
            JavaExtractor::extract_relations(Path::new("OrderService.java"), code, 1, &tree);
        assert!(producers.is_empty());
    }

    #[test]
    fn test_java_kafka_producer_consumer_impact_integration() {
        let mut p = parser();

        let producer_code = r#"
package com.mesh.orders;

public class OrderService {
    public void placeOrder() {
        kafkaTemplate.send("orders.created", "payload");
    }
}
"#;
        let consumer_code = r#"
package com.mesh.notifications;

public class OrderNotifier {
    @KafkaListener(topics = "orders.created")
    public void onOrderCreated(String msg) {}
}
"#;

        let mut graph = ContractGraph::new();

        let producer_tree = p.parse(producer_code, None).expect("parse");
        let (producer_nodes, _deps, producers, _rpc_calls) = JavaExtractor::extract_relations(
            Path::new("services/orders/OrderService.java"),
            producer_code,
            1,
            &producer_tree,
        );
        let producer_ids: Vec<_> = producer_nodes
            .into_iter()
            .map(|n| graph.add_node(n))
            .collect();
        for (i, topic) in producers {
            graph.add_producer(producer_ids[i], topic.as_str());
        }

        let consumer_tree = p.parse(consumer_code, None).expect("parse");
        let (consumer_nodes, _deps2, _producers2, _rpc_calls2) = JavaExtractor::extract_relations(
            Path::new("services/notifications/OrderNotifier.java"),
            consumer_code,
            2,
            &consumer_tree,
        );
        for node in consumer_nodes {
            // Same wiring the Java branch of `languages::mod::PolyglotIndexer::extract`
            // already applies for `@KafkaListener`: a `KafkaTopic`-kind node is a consumer
            // of the topic carried in its own name.
            let is_topic = node.kind == NodeKind::KafkaTopic;
            let topic_name = node.name.clone();
            let id = graph.add_node(node);
            if is_topic {
                graph.add_consumer(id, topic_name.as_str());
            }
        }

        let impact = graph.analyze_impact("orders.created");
        // The producer relation is keyed to the method whose body calls
        // `kafkaTemplate.send(...)` (`placeOrder`), mirroring how the consumer
        // side keys to the specific `@KafkaListener`-annotated method rather
        // than the enclosing class.
        assert!(impact
            .upstream_producers
            .iter()
            .any(|n| n.name == "placeOrder"));
        assert!(!impact.downstream_consumers.is_empty());
    }

    #[test]
    fn test_java_http_annotations_and_getter_isolation() {
        let code = r#"
package com.mesh.api;

public class ResourceController {
    @PutMapping("/items/{id}")
    public void updateItem() {}

    @DeleteMapping("/items/{id}")
    public void deleteItem() {}

    @PatchMapping("/items/{id}")
    public void patchItem() {}

    @GET
    @Path("/jaxrs")
    public String getJaxRs() { return "ok"; }

    @DELETE
    public void deleteJaxRs() {}

    // Lombok or custom getter should not trigger @GET false positive
    @Getter
    public String getName() { return "name"; }
}
"#;
        let mut parser = parser();
        let nodes =
            JavaExtractor::extract(Path::new("ResourceController.java"), code, 1, &mut parser);

        assert!(nodes
            .iter()
            .any(|n| n.name == "updateItem" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "deleteItem" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "patchItem" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "getJaxRs" && n.kind == NodeKind::HttpEndpoint));
        assert!(nodes
            .iter()
            .any(|n| n.name == "deleteJaxRs" && n.kind == NodeKind::HttpEndpoint));

        let getter_node = nodes
            .iter()
            .find(|n| n.name == "getName")
            .expect("getName method found");
        assert_ne!(getter_node.kind, NodeKind::HttpEndpoint);
    }

    #[test]
    fn test_java_kafka_topic_parsing() {
        let code = r#"
package com.mesh.events;

public class EventConsumer {
    @KafkaListener("orders.direct")
    public void handleDirect(String event) {}

    @KafkaListener
    public void handleDynamic(String event) {}
}
"#;
        let mut parser = parser();
        let nodes = JavaExtractor::extract(Path::new("EventConsumer.java"), code, 1, &mut parser);

        assert!(nodes
            .iter()
            .any(|n| n.name == "orders.direct" && n.kind == NodeKind::KafkaTopic));
        assert!(!nodes.iter().any(|n| n.name == "unknown.topic"));
    }
}
