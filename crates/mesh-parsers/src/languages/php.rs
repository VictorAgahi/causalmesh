use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

/// PHP extractor covering plain classes/interfaces plus the two dominant
/// framework conventions: Symfony `#[Route(...)]` attributes and Laravel's
/// `Route::get(...)` facade calls.
pub struct PhpExtractor;

impl PhpExtractor {
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
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

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &package_name,
            false,
            &mut nodes,
        );
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        in_controller: bool,
        nodes: &mut Vec<ContractNode>,
    ) {
        let mut child_in_controller = in_controller;

        match node.kind() {
            "class_declaration" | "interface_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownClass");

                let is_interface = node.kind() == "interface_declaration";
                let is_controller = name.ends_with("Controller");

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(name),
                    kind: if is_interface {
                        NodeKind::Interface
                    } else {
                        NodeKind::ServiceClass
                    },
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(Self::first_line(node, source, name))),
                    docstring: None,
                });
                child_in_controller = is_controller;
            }
            "method_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownMethod");

                let route_from_attribute = Self::route_attribute(node, source);

                let kind = if route_from_attribute.is_some()
                    || (in_controller && name != "__construct")
                {
                    NodeKind::HttpEndpoint
                } else {
                    NodeKind::ServiceClass
                };

                let final_name = route_from_attribute.unwrap_or_else(|| name.to_string());

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(&final_name),
                    kind,
                    file_path: file_path.clone(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(Self::first_line(node, source, name))),
                    docstring: None,
                });
                return;
            }
            "expression_statement" => {
                if let Some(node_out) = Self::laravel_route_node(node, source, package_name, repo_id) {
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
                child_in_controller,
                nodes,
            );
        }
    }

    /// Symfony: `#[Route('/users', name: 'users_index')]` attached to a
    /// `method_declaration`'s `attributes` field.
    fn route_attribute(node: Node, source: &[u8]) -> Option<String> {
        let attrs = node.child_by_field_name("attributes")?;
        let text = attrs.utf8_text(source).ok()?;
        if !text.contains("Route") {
            return None;
        }
        Self::first_string_literal(attrs, source)
    }

    fn first_string_literal(node: Node, source: &[u8]) -> Option<String> {
        if node.kind() == "string" {
            return node
                .utf8_text(source)
                .ok()
                .map(|t| t.trim_matches(|c: char| c == '\'' || c == '"').to_string());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = Self::first_string_literal(child, source) {
                return Some(found);
            }
        }
        None
    }

    /// Laravel: `Route::get('/users', [UserController::class, 'index']);`
    fn laravel_route_node(
        node: Node,
        source: &[u8],
        package_name: &CompactStr,
        repo_id: RepoId,
    ) -> Option<ContractNode> {
        let call = node.named_child(0).filter(|c| c.kind() == "scoped_call_expression")?;
        let scope = call
            .child_by_field_name("scope")
            .and_then(|n| n.utf8_text(source).ok())?;
        if scope != "Route" {
            return None;
        }
        let verb = call
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())?;

        const VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "any", "resource"];
        if !VERBS.contains(&verb) {
            return None;
        }

        let path = call
            .child_by_field_name("arguments")
            .and_then(|args| args.named_child(0))
            .and_then(|arg| Self::first_string_literal(arg, source))?;

        let name = format!("{} {}", verb.to_uppercase(), path);
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
        let lang = tree_sitter_php::LANGUAGE_PHP.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_php_extractor_controller() {
        let code = r#"<?php
class UserController extends AbstractController {
    public function __construct() {}

    public function index() {
        return new Response();
    }
}
"#;
        let mut p = parser();
        let nodes = PhpExtractor::extract(Path::new("src/Controller/UserController.php"), code, 1, &mut p);
        let find = |name: &str| nodes.iter().find(|n| n.name == name);
        assert_eq!(find("UserController").unwrap().kind, NodeKind::ServiceClass);
        assert_eq!(find("index").unwrap().kind, NodeKind::HttpEndpoint);
        assert_eq!(find("__construct").unwrap().kind, NodeKind::ServiceClass);
    }

    #[test]
    fn test_php_extractor_interface() {
        let code = "<?php\ninterface Foo { public function bar($x); }\n";
        let mut p = parser();
        let nodes = PhpExtractor::extract(Path::new("Foo.php"), code, 0, &mut p);
        assert_eq!(nodes.iter().find(|n| n.name == "Foo").unwrap().kind, NodeKind::Interface);
    }

    #[test]
    fn test_php_extractor_symfony_route_attribute() {
        let code = r#"<?php
class UserController extends AbstractController {
    #[Route('/users', name: 'users_index')]
    public function index() {
        return new Response();
    }
}
"#;
        let mut p = parser();
        let nodes = PhpExtractor::extract(Path::new("UserController.php"), code, 0, &mut p);
        assert!(nodes
            .iter()
            .any(|n| n.name == "/users" && n.kind == NodeKind::HttpEndpoint));
    }

    #[test]
    fn test_php_extractor_laravel_route() {
        let code = "<?php\nRoute::get('/users', [UserController::class, 'index']);\n";
        let mut p = parser();
        let nodes = PhpExtractor::extract(Path::new("routes/web.php"), code, 0, &mut p);
        assert!(nodes
            .iter()
            .any(|n| n.name == "GET /users" && n.kind == NodeKind::HttpEndpoint));
    }
}
