use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

/// Scala extractor. `class`/`object` map to `ServiceClass`, `trait` to
/// `Interface`, and Akka HTTP's `path("segment") { get { ... } }` route DSL
/// is recognised as `HttpEndpoint`.
pub struct ScalaExtractor;

impl ScalaExtractor {
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

        Self::visit_node(root, source_bytes, &file_path, repo_id, &package_name, &mut nodes);
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
            "class_definition" | "trait_definition" | "object_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownType");

                let kind = if node.kind() == "trait_definition" {
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
            "call_expression" => {
                Self::emit_akka_routes(node, source, file_path, repo_id, package_name, nodes);
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, file_path, repo_id, package_name, nodes);
        }
    }

    /// Recognises `path("segment") { get { ... } }` / `pathPrefix(...)`.
    fn emit_akka_routes(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "call_expression" {
            return;
        }
        let Some(inner_name) = func
            .child_by_field_name("function")
            .and_then(|n| n.utf8_text(source).ok())
        else {
            return;
        };
        if inner_name != "path" && inner_name != "pathPrefix" && inner_name != "pathEnd" {
            return;
        }
        let Some(args) = func.child_by_field_name("arguments") else {
            return;
        };
        let Some(path_lit) = Self::first_string_literal(args, source) else {
            return;
        };
        let Some(body) = node.child_by_field_name("arguments") else {
            return;
        };

        let mut verbs = Vec::new();
        Self::collect_verbs(body, source, &mut verbs);

        let emit_names: Vec<String> = if verbs.is_empty() {
            vec![format!("ANY {path_lit}")]
        } else {
            verbs
                .into_iter()
                .map(|v| format!("{} {}", v.to_uppercase(), path_lit))
                .collect()
        };

        for name in emit_names {
            nodes.push(ContractNode {
                id: 0,
                name: CompactStr::new(&name),
                kind: NodeKind::HttpEndpoint,
                file_path: file_path.clone(),
                line_start: node.start_position().row + 1,
                line_end: node.end_position().row + 1,
                package: package_name.clone(),
                repo_id,
                signature: Some(CompactStr::new(&name)),
                docstring: None,
            });
        }
    }

    fn collect_verbs<'a>(node: Node<'a>, source: &'a [u8], out: &mut Vec<&'a str>) {
        const VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
        if node.kind() == "call_expression" {
            if let Some(name) = node
                .child_by_field_name("function")
                .and_then(|n| n.utf8_text(source).ok())
            {
                if VERBS.contains(&name) && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_verbs(child, source, out);
        }
    }

    fn first_string_literal(node: Node, source: &[u8]) -> Option<String> {
        if node.kind() == "string" {
            return node
                .utf8_text(source)
                .ok()
                .map(|t| t.trim_matches('"').to_string());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = Self::first_string_literal(child, source) {
                return Some(found);
            }
        }
        None
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
        let lang = tree_sitter_scala::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_scala_extractor_class_trait_object() {
        let code = r#"
trait Payable {
  def charge(amount: Int): Boolean
}

class PaymentService extends Payable {
  def charge(amount: Int): Boolean = {
    amount > 0
  }
}

object PaymentApp {
  def main(args: Array[String]): Unit = ()
}
"#;
        let mut p = parser();
        let nodes = ScalaExtractor::extract(Path::new("src/main/scala/PaymentService.scala"), code, 5, &mut p);
        let find = |name: &str| nodes.iter().find(|n| n.name == name);
        assert_eq!(find("Payable").unwrap().kind, NodeKind::Interface);
        assert_eq!(find("PaymentService").unwrap().kind, NodeKind::ServiceClass);
        assert_eq!(find("PaymentApp").unwrap().kind, NodeKind::ServiceClass);
        assert!(nodes.iter().all(|n| n.repo_id == 5));
    }

    #[test]
    fn test_scala_extractor_akka_route() {
        let code = r#"
object Routes {
  val route = path("users") {
    get {
      complete("ok")
    }
  }
}
"#;
        let mut p = parser();
        let nodes = ScalaExtractor::extract(Path::new("src/main/scala/Routes.scala"), code, 0, &mut p);
        assert!(nodes
            .iter()
            .any(|n| n.name == "GET users" && n.kind == NodeKind::HttpEndpoint));
    }
}
