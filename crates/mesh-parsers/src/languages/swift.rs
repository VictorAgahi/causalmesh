use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Tree};

/// Swift extractor. `class_declaration` covers `class`/`struct`/`extension` in
/// this grammar (all map to `ServiceClass`), `protocol_declaration` maps to
/// `Interface`, and `app.get("/path") { ... }`-style Vapor route
/// registrations are recognised as `HttpEndpoint`.
pub struct SwiftExtractor;

impl SwiftExtractor {
    /// Extracts from an already-parsed tree. `PolyglotIndexer` (production
    /// indexing) parses once via `AstGuard::parse_with`, so a parse failure is
    /// visible instead of silently producing an empty result indistinguishable
    /// from a legitimately empty file; tests parse `content` themselves first.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let package_name = mesh_core::detect_service_package(&file_path, None);

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            &mut nodes,
            0,
        );
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

        match node.kind() {
            "class_declaration" | "protocol_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownType");

                let kind = if node.kind() == "protocol_declaration" {
                    NodeKind::Interface
                } else {
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
                    signature: Some(CompactStr::new(Self::first_line(node, source, name))),
                    docstring: None,
                });
            }
            "function_declaration" | "protocol_function_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownFunc");

                let kind = if name.starts_with("handle") || name.ends_with("Handler") {
                    NodeKind::HttpEndpoint
                } else {
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
                    signature: Some(CompactStr::new(Self::first_line(node, source, name))),
                    docstring: None,
                });
            }
            "call_expression" => {
                if let Some(node_out) = Self::vapor_route_node(node, source, package_name, repo_id)
                {
                    nodes.push(ContractNode {
                        id: 0,
                        file_path: file_path.clone(),
                        line_start: node.start_position().row + 1,
                        line_end: node.end_position().row + 1,
                        ..node_out
                    });
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
                depth + 1,
            );
        }
    }

    /// Recognises `app.get("/path") { ... }` / `router.post("/path") { ... }`.
    fn vapor_route_node(
        node: Node,
        source: &[u8],
        package_name: &CompactStr,
        repo_id: RepoId,
    ) -> Option<ContractNode> {
        let nav = Self::find_child_of_kind(node, "navigation_expression")?;
        let suffix = nav
            .child_by_field_name("suffix")
            .and_then(|s| s.utf8_text(source).ok())?
            .trim_start_matches('.');

        const VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
        if !VERBS.contains(&suffix) {
            return None;
        }

        let call_suffix = Self::find_child_of_kind(node, "call_suffix")?;
        let value_args = Self::find_child_of_kind(call_suffix, "value_arguments")?;
        let first_arg = value_args.named_child(0)?;
        let literal = first_arg
            .child_by_field_name("value")
            .or_else(|| first_arg.named_child(0))?;
        let path_text = literal.utf8_text(source).ok()?;
        let path = path_text.trim_matches(|c: char| c == '"');

        let name = format!("{} {}", suffix.to_uppercase(), path);
        Some(ContractNode {
            id: 0,
            name: CompactStr::new(&name),
            kind: NodeKind::HttpEndpoint,
            file_path: Arc::from(Path::new("")),
            line_start: 0,
            line_end: 0,
            package: package_name.clone(),
            repo_id,
            signature: Some(CompactStr::new(&name)),
            docstring: None,
        })
    }

    fn find_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        let found = node.children(&mut cursor).find(|c| c.kind() == kind);
        found
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
    use tree_sitter::Parser;

    fn parser() -> Parser {
        let mut parser = Parser::new();
        let lang = tree_sitter_swift::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_swift_extractor_class_and_protocol() {
        let code = r#"
protocol Payable {
    func charge(amount: Int) -> Bool
}

class PaymentService: Payable {
    func charge(amount: Int) -> Bool {
        return amount > 0
    }
}

struct Invoice {
    var total: Int
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes = SwiftExtractor::extract(
            Path::new("Sources/App/PaymentService.swift"),
            code,
            4,
            &tree,
        );
        let find = |name: &str| nodes.iter().find(|n| n.name == name);
        assert_eq!(find("Payable").unwrap().kind, NodeKind::Interface);
        assert_eq!(find("PaymentService").unwrap().kind, NodeKind::ServiceClass);
        assert_eq!(find("Invoice").unwrap().kind, NodeKind::ServiceClass);
        assert!(nodes.iter().all(|n| n.repo_id == 4));
    }

    #[test]
    fn test_swift_extractor_vapor_route() {
        let code = r#"
import Vapor
func routes(_ app: Application) throws {
    app.get("users") { req in
        return "ok"
    }
}
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let nodes = SwiftExtractor::extract(Path::new("Sources/App/routes.swift"), code, 0, &tree);
        assert!(nodes
            .iter()
            .any(|n| n.name == "GET users" && n.kind == NodeKind::HttpEndpoint));
    }
}
