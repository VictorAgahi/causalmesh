use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct RustExtractor;

impl RustExtractor {
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
        nodes: &mut Vec<ContractNode>,
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
        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

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
        let mut parser = Parser::new();
        let lang = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let nodes = RustExtractor::extract(Path::new("src/trading.rs"), code, 2, &mut parser);
        assert!(
            nodes.iter().any(|n| n.name == "fintech.orders"),
            "Expected fintech.orders contract node from tonic macro"
        );
    }
}
