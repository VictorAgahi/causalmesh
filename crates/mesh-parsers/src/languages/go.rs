use mesh_core::{CompactStr, ContractNode, NodeKind, RepoId};
use std::path::Path;
use tree_sitter::{Node, Parser};

pub struct GoExtractor;

impl GoExtractor {
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
    ) -> Vec<ContractNode> {
        let mut nodes = Vec::new();
        let tree = match parser.parse(content, None) {
            Some(t) => t,
            None => return nodes,
        };

        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = CompactStr::default();

        Self::visit_node(
            root,
            source_bytes,
            file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
        );
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &Path,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        match node.kind() {
            "package_clause" => {
                if let Ok(text) = node.utf8_text(source) {
                    let clean = text.trim_start_matches("package ").trim();
                    *package_name = CompactStr::new(clean);
                }
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
                            file_path: file_path.to_path_buf(),
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

                let mut kind = NodeKind::ServiceClass;
                if func_name.starts_with("Handle") || func_name.ends_with("Handler") {
                    kind = NodeKind::HttpEndpoint;
                }

                nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(func_name),
                    kind,
                    file_path: file_path.to_path_buf(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    package: package_name.clone(),
                    repo_id,
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, file_path, repo_id, package_name, nodes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_go_extractor() {
        let code = r#"
package auth

type Server struct {}

func (s *Server) AuthenticateUser(ctx context.Context, req *AuthRequest) (*AuthResponse, error) {
    return nil, nil
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_go::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let nodes = GoExtractor::extract(Path::new("server.go"), code, 2, &mut parser);
        assert!(nodes.iter().any(|n| n.name == "Server"));
        assert!(nodes.iter().any(|n| n.name == "AuthenticateUser"));
    }
}
