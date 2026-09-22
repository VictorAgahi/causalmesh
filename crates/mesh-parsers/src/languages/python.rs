use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct PythonExtractor;

impl PythonExtractor {
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
                if class_name.ends_with("Servicer") {
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
                    signature: Some(CompactStr::new(first_line)),
                    docstring: None,
                });
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
                // Check previous siblings for decorators
                if let Some(prev) = node.prev_sibling() {
                    if prev.kind() == "decorator" {
                        if let Ok(dec_text) = prev.utf8_text(source) {
                            if dec_text.contains("@app.") || dec_text.contains("@router.") {
                                kind = NodeKind::HttpEndpoint;
                            } else if dec_text.contains("@task") || dec_text.contains("@celery") {
                                kind = NodeKind::Queue;
                            }
                        }
                    }
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
    fn test_python_extractor() {
        let code = r#"
class UserServicer:
    def GetUser(self, request, context):
        pass

@app.get("/users")
def list_users():
    return []
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_python::LANGUAGE.into();
        parser.set_language(&lang).unwrap();

        let nodes = PythonExtractor::extract(Path::new("services/user.py"), code, 3, &mut parser);
        assert!(nodes
            .iter()
            .any(|n| n.name == "UserServicer" && n.kind == NodeKind::GrpcService));
        assert!(nodes.iter().any(|n| n.name == "GetUser"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "list_users" && n.kind == NodeKind::HttpEndpoint));
    }
}
