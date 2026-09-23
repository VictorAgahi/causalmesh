use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

use super::FileIndex;

pub struct RustExtractor;

impl RustExtractor {
    /// Extracts contract nodes only. Kept for callers that don't need dependency /
    /// producer / consumer edges (`extract_index` is the richer entry point).
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        Self::extract_index(file_path, content, repo_id, parser).nodes
    }

    /// Extracts contract nodes plus `use`/`extern crate` dependencies and native
    /// `rdkafka` producer/consumer relations, ready to `apply` onto a `ContractGraph`.
    pub fn extract_index(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> FileIndex {
        let file_path: FilePath = Arc::from(file_path);
        let mut out = FileIndex::default();
        let mut imports: Vec<(String, String)> = Vec::new();

        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return out,
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);

        Self::visit_children(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            &mut out.nodes,
            &mut imports,
            &mut out.producers,
            &mut out.consumers,
            false,
        );

        Self::attach_dependencies(content, &out.nodes, &imports, &mut out.dependencies);
        out
    }

    /// Reuses the "is it actually used in this node's line range?" heuristic from
    /// `languages/mod.rs`'s TypeScript handling: a qualified `a::b::C` path behaves
    /// like a module-path import (applies file-wide), while a bare last-segment name
    /// must actually appear in the node's own source range.
    fn attach_dependencies(
        content: &str,
        nodes: &[ContractNode],
        imports: &[(String, String)],
        dependencies: &mut Vec<(usize, CompactStr)>,
    ) {
        if imports.is_empty() {
            return;
        }
        let content_lines: Vec<&str> = content.lines().collect();
        for (i, node) in nodes.iter().enumerate() {
            let body = content_lines
                .get(node.line_start.saturating_sub(1)..node.line_end)
                .unwrap_or(&[]);
            for (_, imported) in imports {
                if imported.is_empty() {
                    continue;
                }
                let is_module_path_entry = imported.contains("::");
                let is_used =
                    is_module_path_entry || body.iter().any(|l| l.contains(imported.as_str()));
                if is_used {
                    dependencies.push((i, CompactStr::new(imported.as_str())));
                }
            }
        }
    }

    /// Iterates `node`'s direct children, accumulating `#[...]` attribute text so it
    /// can be handed to the next non-attribute sibling (e.g. `#[tonic::async_trait]`
    /// on an `impl`, `#[get("/path")]` on a `fn`). `in_grpc_body` marks that these
    /// children are direct members of a tonic service `impl` block.
    #[allow(clippy::too_many_arguments)]
    fn visit_children(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<(String, String)>,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
        in_grpc_body: bool,
    ) {
        let mut cursor = node.walk();
        let mut pending_attr = String::new();
        for child in node.children(&mut cursor) {
            if child.kind() == "attribute_item" {
                if let Ok(t) = child.utf8_text(source) {
                    pending_attr.push_str(t);
                    pending_attr.push('\n');
                }
                continue;
            }
            Self::visit_node(
                child,
                source,
                file_path,
                repo_id,
                package_name,
                nodes,
                imports,
                producers,
                consumers,
                &pending_attr,
                in_grpc_body,
            );
            pending_attr.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<(String, String)>,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
        pending_attr: &str,
        in_grpc_body: bool,
    ) {
        match node.kind() {
            "struct_item" | "enum_item" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownItem");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("struct {name}"));

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
            "trait_item" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownTrait");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("trait {name}"));

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(name),
                    kind: NodeKind::Interface,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
            }
            "function_item" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknown_fn");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("fn {name}()"));

                if in_grpc_body {
                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(name),
                        kind: NodeKind::GrpcMethod,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                } else if let Some((method, path)) = Self::actix_route_from_attr(pending_attr) {
                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(format!("{method} {path}")),
                        kind: NodeKind::HttpEndpoint,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        package: package_name.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                } else {
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

                // The node just pushed above is this function's own entry — attach
                // any native rdkafka producer/consumer relations found in its body
                // to it, and detect any axum `Router::route(...)` registrations
                // nested inside it (pushed as separate `HttpEndpoint` nodes).
                let idx = nodes.len() - 1;
                if let Ok(text) = node.utf8_text(source) {
                    Self::scan_rdkafka(text, idx, producers, consumers);
                    Self::scan_axum_routes(
                        text,
                        node.start_position().row,
                        file_path,
                        repo_id,
                        package_name,
                        nodes,
                    );
                }
            }
            "impl_item" => {
                let is_grpc =
                    Self::handle_impl(node, source, file_path, repo_id, package_name, nodes, pending_attr);
                // The impl's methods live one level down, inside its
                // `declaration_list` body — recurse into *that* with the
                // gRPC flag, not into the `impl_item` itself (whose direct
                // children are the trait/type paths, not the methods).
                if let Some(body) = node
                    .children(&mut node.walk())
                    .find(|c| c.kind() == "declaration_list")
                {
                    Self::visit_children(
                        body,
                        source,
                        file_path,
                        repo_id,
                        package_name,
                        nodes,
                        imports,
                        producers,
                        consumers,
                        is_grpc,
                    );
                }
                return;
            }
            "use_declaration" => {
                Self::collect_use_paths(node, source, imports);
            }
            "extern_crate_declaration" => {
                if let Ok(text) = node.utf8_text(source) {
                    let inner = text
                        .trim()
                        .trim_start_matches("extern")
                        .trim()
                        .trim_start_matches("crate")
                        .trim()
                        .trim_end_matches(';')
                        .trim();
                    let name = inner.split(" as ").next().unwrap_or(inner).trim();
                    if !name.is_empty() {
                        imports.push((String::new(), name.to_string()));
                    }
                }
            }
            "macro_invocation" => {
                if let Ok(text) = node.utf8_text(source) {
                    if text.contains("include_proto!") {
                        if let Some(open) = text.find('(') {
                            if let Some(close) = text[open..].find(')') {
                                let arg = text[open + 1..open + close]
                                    .trim()
                                    .trim_matches('"')
                                    .trim_matches('\'');
                                if !arg.is_empty() {
                                    nodes.push(ContractNode {
                                        id: 0,
                                        name: CompactStr::new(arg),
                                        kind: NodeKind::ServiceClass,
                                        file_path: file_path.clone(),
                                        line_start: node.start_position().row + 1,
                                        line_end: node.end_position().row + 1,
                                        package: package_name.clone(),
                                        repo_id,
                                        signature: Some(CompactStr::new(format!(
                                            "tonic::include_proto!(\"{arg}\")"
                                        ))),
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

        Self::visit_children(
            node,
            source,
            file_path,
            repo_id,
            package_name,
            nodes,
            imports,
            producers,
            consumers,
            false,
        );
    }

    /// Recognises a `#[tonic::async_trait]`-annotated (or `*Server`/`*Service`
    /// named trait) `impl Trait for Struct` block, pushing a `GrpcService` node.
    /// Returns whether the impl was classified as a gRPC service, so its direct
    /// method children can be pushed as `GrpcMethod` instead of `ServiceClass`.
    #[allow(clippy::too_many_arguments)]
    fn handle_impl(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
        pending_attr: &str,
    ) -> bool {
        let Ok(full_text) = node.utf8_text(source) else {
            return false;
        };
        let first_line = full_text.lines().next().unwrap_or(full_text).trim();

        let Some(trait_name) = first_line.strip_prefix("impl").and_then(|rest| {
            rest.trim()
                .split_once(" for ")
                .map(|(tr, _)| tr.trim().split('<').next().unwrap_or(tr).trim().to_string())
        }) else {
            return false;
        };
        if trait_name.is_empty() {
            return false;
        }

        let is_grpc = pending_attr.contains("tonic::async_trait")
            || trait_name.ends_with("Server")
            || trait_name.ends_with("Service");
        if !is_grpc {
            return false;
        }

        nodes.push(ContractNode {
            id: 0,
            name: CompactStr::new(trait_name.as_str()),
            kind: NodeKind::GrpcService,
            file_path: file_path.clone(),
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            package: package_name.clone(),
            repo_id,
            signature: Some(CompactStr::new(first_line)),
            docstring: None,
        });
        true
    }

    /// Native `rdkafka` detection: `.subscribe(&["topic"])` calls as consumers,
    /// `FutureRecord::to("topic")` (the argument to `.send(...)`) as producers.
    fn scan_rdkafka(
        text: &str,
        idx: usize,
        producers: &mut Vec<(usize, CompactStr)>,
        consumers: &mut Vec<(usize, CompactStr)>,
    ) {
        if let Some(pos) = text.find(".subscribe(") {
            let after = &text[pos + ".subscribe(".len()..];
            let end = after.find(')').unwrap_or(after.len());
            for topic in Self::extract_quoted_strings(&after[..end]) {
                consumers.push((idx, CompactStr::new(topic)));
            }
        }

        const NEEDLE: &str = "FutureRecord::to(";
        for (rel, _) in text.match_indices(NEEDLE) {
            let after = &text[rel + NEEDLE.len()..];
            let end = after.find(')').unwrap_or(after.len());
            if let Some(topic) = Self::extract_quoted_strings(&after[..end]).into_iter().next() {
                producers.push((idx, CompactStr::new(topic)));
            }
        }
    }

    /// Native `axum` detection: `Router::route("/path", get(handler))` style route
    /// registration, pushed as new `HttpEndpoint` nodes.
    fn scan_axum_routes(
        text: &str,
        base_row: usize,
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        const NEEDLE: &str = ".route(";
        for (abs, _) in text.match_indices(NEEDLE) {
            let after = &text[abs + NEEDLE.len()..];
            let Some(path) = Self::extract_quoted_strings(after).into_iter().next() else {
                continue;
            };
            let method = Self::extract_axum_method(after).unwrap_or_else(|| "ANY".to_string());
            let line = base_row + text[..abs].matches('\n').count() + 1;

            nodes.push(ContractNode {
                id: 0,
                name: CompactStr::new(format!("{method} {path}")),
                kind: NodeKind::HttpEndpoint,
                file_path: file_path.clone(),
                line_start: line,
                line_end: line,
                package: package_name.clone(),
                repo_id,
                signature: Some(CompactStr::new(format!(
                    "route(\"{path}\", {}(..))",
                    method.to_lowercase()
                ))),
                docstring: None,
            });
        }
    }

    /// Extracts the HTTP-method identifier (`get`, `post`, ...) that follows a
    /// `route("/path", <method>(handler))` call's path argument.
    fn extract_axum_method(after_route_open_paren: &str) -> Option<String> {
        let comma = after_route_open_paren.find(',')?;
        let rest = after_route_open_paren[comma + 1..].trim_start();
        let end = rest.find('(')?;
        let ident = rest[..end].trim();
        let last = ident.rsplit("::").next().unwrap_or(ident);
        if last.is_empty() || !last.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            None
        } else {
            Some(last.to_uppercase())
        }
    }

    /// Recognises `#[get("/path")]`-style actix-web method attribute macros in
    /// accumulated attribute text, returning `(METHOD, path)`.
    fn actix_route_from_attr(attr: &str) -> Option<(String, String)> {
        const METHODS: [&str; 7] = ["get", "post", "put", "delete", "patch", "head", "options"];
        for m in METHODS {
            let needle = format!("#[{m}(");
            if let Some(idx) = attr.find(&needle) {
                let rest = &attr[idx + needle.len()..];
                if let Some(path) = Self::extract_quoted_strings(rest).into_iter().next() {
                    if !path.is_empty() {
                        return Some((m.to_uppercase(), path));
                    }
                }
            }
        }
        None
    }

    /// Returns every quoted substring in `s`, in order, without the surrounding
    /// quotes. Used across the rdkafka/axum/actix scanners above.
    fn extract_quoted_strings(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find('"') {
            let after_start = &rest[start + 1..];
            let Some(end) = after_start.find('"') else {
                break;
            };
            out.push(after_start[..end].to_string());
            rest = &after_start[end + 1..];
        }
        out
    }

    /// Parses a `use` declaration's text (without the leading `use` / trailing
    /// `;`) into fully-qualified dependency paths, pushing both the raw `::`
    /// path and its last segment into `imports` (mirroring the TypeScript
    /// extractor's dual push of module path + named specifier in
    /// `languages/mod.rs`).
    fn collect_use_paths(node: Node, source: &[u8], imports: &mut Vec<(String, String)>) {
        let Ok(text) = node.utf8_text(source) else {
            return;
        };
        let inner = text
            .trim()
            .trim_start_matches("use")
            .trim()
            .trim_end_matches(';')
            .trim();

        let mut paths = Vec::new();
        Self::expand_use_tree(inner, "", &mut paths);

        for full in paths {
            if full.is_empty() {
                continue;
            }
            let last = full.rsplit("::").next().unwrap_or(full.as_str()).to_string();
            imports.push((String::new(), full.clone()));
            if last != full {
                imports.push((String::new(), last));
            }
        }
    }

    /// Recursively expands a (possibly nested) `use` tree, e.g. `a::{b, c::D}`,
    /// into fully-qualified paths: `["a::b", "a::c::D"]`.
    fn expand_use_tree(text: &str, prefix: &str, out: &mut Vec<String>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }

        if let Some(brace_pos) = text.find('{') {
            let pre = text[..brace_pos].trim().trim_end_matches("::").trim();
            let new_prefix = match (prefix.is_empty(), pre.is_empty()) {
                (true, _) => pre.to_string(),
                (false, true) => prefix.to_string(),
                (false, false) => format!("{prefix}::{pre}"),
            };
            if let Some(inner) = Self::braces_inner(text, brace_pos) {
                for part in Self::split_top_level_commas(inner) {
                    Self::expand_use_tree(part, &new_prefix, out);
                }
            }
            return;
        }

        let path = text.split(" as ").next().unwrap_or(text).trim();
        if path.is_empty() || path == "self" || path == "*" {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
            return;
        }

        let full = if prefix.is_empty() {
            path.to_string()
        } else {
            format!("{prefix}::{path}")
        };
        out.push(full);
    }

    /// Returns the text strictly between the `{` at `start` and its matching `}`.
    fn braces_inner(s: &str, start: usize) -> Option<&str> {
        let bytes = s.as_bytes();
        let mut depth = 0i32;
        for (i, b) in bytes.iter().enumerate().skip(start) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[start + 1..i]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Splits `s` on commas that are not nested inside `{}`.
    fn split_top_level_commas(s: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(s[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(s[start..].trim());
        parts.into_iter().filter(|p| !p.is_empty()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::ContractGraph;

    fn new_parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_rust_extractor() {
        let code = r#"
pub struct AuthServer {
    db: Arc<Db>,
}

impl AuthServer {
    pub async fn authenticate(&self) -> Result<Token, Error> {
        Ok(Token)
    }
}
"#;
        let mut parser = new_parser();
        let nodes = RustExtractor::extract(Path::new("src/auth.rs"), code, 5, &mut parser);
        assert!(nodes.iter().any(|n| n.name == "AuthServer"));
        assert!(nodes.iter().any(|n| n.name == "authenticate"));
    }

    #[test]
    fn test_rust_tonic_macro_extractor() {
        let code = r#"
pub mod proto {
    tonic::include_proto!("fintech.orders");
}
"#;
        let mut parser = new_parser();
        let nodes = RustExtractor::extract(Path::new("src/trading.rs"), code, 2, &mut parser);
        assert!(
            nodes.iter().any(|n| n.name == "fintech.orders"),
            "Expected fintech.orders contract node from tonic macro"
        );
    }

    // --- Item 2: `use` / `extern crate` dependency extraction ---------------

    #[test]
    fn test_use_declaration_dependency_find_dependents() {
        let mut graph = ContractGraph::new();
        let mut parser = new_parser();

        let producer_code = r#"
pub struct Widget;
"#;
        let consumer_code = r#"
use crate::widget::Widget;

pub fn make() -> Widget {
    Widget
}
"#;

        RustExtractor::extract_index(Path::new("src/widget.rs"), producer_code, 1, &mut parser)
            .apply(&mut graph);
        RustExtractor::extract_index(Path::new("src/consumer.rs"), consumer_code, 1, &mut parser)
            .apply(&mut graph);

        let dependents = graph.find_dependents("Widget");
        assert!(
            dependents.iter().any(|n| n.name == "make"),
            "expected `make` (which uses `Widget`) to be a dependent, got: {dependents:?}"
        );
    }

    #[test]
    fn test_nested_use_group_dependency() {
        let mut parser = new_parser();
        let code = r#"
use a::{b, c::D};

fn uses_both() {
    b();
    D::new();
}
"#;
        let idx = RustExtractor::extract_index(Path::new("src/lib.rs"), code, 1, &mut parser);
        let dep_targets: Vec<&str> = idx
            .dependencies
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(dep_targets.contains(&"a::b"));
        assert!(dep_targets.contains(&"a::c::D"));
        assert!(dep_targets.contains(&"b"));
        assert!(dep_targets.contains(&"D"));
    }

    #[test]
    fn test_extern_crate_dependency() {
        let mut parser = new_parser();
        let code = r#"
extern crate serde_json;

fn parse() {
    serde_json::from_str("{}").unwrap();
}
"#;
        let idx = RustExtractor::extract_index(Path::new("src/lib.rs"), code, 1, &mut parser);
        assert!(idx
            .dependencies
            .iter()
            .any(|(_, t)| t.as_str() == "serde_json"));
    }

    // --- Item 3: native rdkafka producer/consumer detection -------------------

    #[test]
    fn test_rdkafka_producer_consumer_analyze_impact() {
        let mut graph = ContractGraph::new();
        let mut parser = new_parser();

        let producer_code = r#"
pub async fn publish_order(producer: &FutureProducer) {
    producer
        .send(FutureRecord::to("orders.created").payload("x"), Duration::from_secs(0))
        .await;
}
"#;
        let consumer_code = r#"
pub async fn consume_orders(consumer: &StreamConsumer) {
    consumer.subscribe(&["orders.created"]).unwrap();
}
"#;

        RustExtractor::extract_index(Path::new("src/producer.rs"), producer_code, 1, &mut parser)
            .apply(&mut graph);
        RustExtractor::extract_index(Path::new("src/consumer.rs"), consumer_code, 1, &mut parser)
            .apply(&mut graph);

        let impact = graph.analyze_impact("orders.created");
        assert!(
            !impact.upstream_producers.is_empty(),
            "expected a native rdkafka producer with no custom pattern configured"
        );
        assert!(
            !impact.downstream_consumers.is_empty(),
            "expected a native rdkafka consumer with no custom pattern configured"
        );
    }

    // --- Item 5: extractor parity --------------------------------------------

    #[test]
    fn test_tonic_grpc_service_and_method() {
        let mut parser = new_parser();
        let code = r#"
#[tonic::async_trait]
impl Greeter for MyGreeterService {
    async fn say_hello(&self, request: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        Ok(Response::new(HelloReply::default()))
    }
}
"#;
        let nodes = RustExtractor::extract(Path::new("src/greeter.rs"), code, 1, &mut parser);
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "Greeter" && n.kind == NodeKind::GrpcService),
            "expected a GrpcService node for the tonic impl, got: {nodes:?}"
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "say_hello" && n.kind == NodeKind::GrpcMethod),
            "expected a GrpcMethod node for the tonic method, got: {nodes:?}"
        );
    }

    #[test]
    fn test_axum_router_route_http_endpoint() {
        let mut parser = new_parser();
        let code = r#"
fn app() -> Router {
    Router::new().route("/orders", get(list_orders))
}
"#;
        let nodes = RustExtractor::extract(Path::new("src/app.rs"), code, 1, &mut parser);
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "GET /orders" && n.kind == NodeKind::HttpEndpoint),
            "expected an HttpEndpoint node for the axum route, got: {nodes:?}"
        );
    }

    #[test]
    fn test_actix_web_get_attribute_http_endpoint() {
        let mut parser = new_parser();
        let code = r#"
#[get("/orders")]
async fn list_orders() -> impl Responder {
    HttpResponse::Ok()
}
"#;
        let nodes = RustExtractor::extract(Path::new("src/handlers.rs"), code, 1, &mut parser);
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "GET /orders" && n.kind == NodeKind::HttpEndpoint),
            "expected an HttpEndpoint node for the actix-web attribute, got: {nodes:?}"
        );
    }

    #[test]
    fn test_trait_declaration_interface() {
        let mut parser = new_parser();
        let code = r#"
pub trait PaymentGateway {
    fn charge(&self, amount: u64) -> Result<(), Error>;
}
"#;
        let nodes = RustExtractor::extract(Path::new("src/gateway.rs"), code, 1, &mut parser);
        assert!(
            nodes
                .iter()
                .any(|n| n.name == "PaymentGateway" && n.kind == NodeKind::Interface),
            "expected an Interface node for the trait declaration, got: {nodes:?}"
        );
    }
}
