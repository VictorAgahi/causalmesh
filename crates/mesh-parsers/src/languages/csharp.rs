use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct CSharpExtractor;

impl CSharpExtractor {
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
        let mut package_name = mesh_core::detect_service_package(&file_path, None);

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &mut nodes,
        );
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        nodes: &mut Vec<ContractNode>,
    ) {
        match node.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    *package_name = mesh_core::detect_service_package(file_path, Some(name));
                }
            }
            "class_declaration" | "interface_declaration" | "struct_declaration"
            | "record_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let first_line = Self::first_line(node, source, name);

                    let kind = if node.kind() == "interface_declaration" {
                        NodeKind::Interface
                    } else {
                        // ASP.NET controllers ([ApiController]/[Controller]) and plain
                        // classes both surface as ServiceClass; the attribute is kept
                        // in the signature line rather than a separate NodeKind, since
                        // MeshMCP has no `Controller` variant of its own.
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
                        signature: Some(CompactStr::new(first_line)),
                        docstring: None,
                    });
                }
            }
            "method_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    let attrs = Self::attribute_lists_text(node, source);

                    let kind = if attrs.contains("[HttpGet")
                        || attrs.contains("[HttpPost")
                        || attrs.contains("[HttpPut")
                        || attrs.contains("[HttpDelete")
                        || attrs.contains("[HttpPatch")
                        || attrs.contains("[Route")
                    {
                        NodeKind::HttpEndpoint
                    } else {
                        NodeKind::ServiceClass
                    };

                    let first_line = Self::first_line(node, source, name);

                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(name),
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
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, file_path, repo_id, package_name, nodes);
        }
    }

    /// Attribute lists (`[ApiController]`, `[HttpGet("/x")]`) are direct children of the
    /// declaration node in the C# grammar (siblings of `name`/`body`, not wrapped inside
    /// either), so this concatenates every `attribute_list` child's text.
    fn attribute_lists_text(node: Node, source: &[u8]) -> String {
        let mut text = String::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "attribute_list" {
                if let Ok(t) = child.utf8_text(source) {
                    text.push_str(t);
                    text.push(' ');
                }
            }
        }
        text
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
        let lang = tree_sitter_c_sharp::language();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_csharp_extractor_controller_and_interface() {
        let code = r#"
namespace Mesh.Billing;

public interface IBillingService
{
    Task<Bill> GetBill(int id);
}

[ApiController]
[Route("api/[controller]")]
public class BillingController : ControllerBase
{
    [HttpGet("/bills")]
    public IActionResult GetBills()
    {
        return Ok();
    }

    [HttpPost("/bills")]
    public IActionResult CreateBill()
    {
        return Ok();
    }
}
"#;
        let mut p = parser();
        let nodes = CSharpExtractor::extract(
            Path::new("services/billing/BillingController.cs"),
            code,
            2,
            &mut p,
        );

        let find = |name: &str| nodes.iter().find(|n| n.name == name);

        assert_eq!(
            find("IBillingService").expect("interface").kind,
            NodeKind::Interface
        );
        assert_eq!(
            find("BillingController").expect("controller class").kind,
            NodeKind::ServiceClass
        );
        assert_eq!(
            find("GetBills").expect("http get").kind,
            NodeKind::HttpEndpoint
        );
        assert_eq!(
            find("CreateBill").expect("http post").kind,
            NodeKind::HttpEndpoint
        );
        assert!(nodes.iter().all(|n| n.repo_id == 2));
        assert_eq!(
            find("BillingController").unwrap().package.as_str(),
            "billing"
        );
    }

    #[test]
    fn test_csharp_extractor_plain_method_is_service_class() {
        let code = r#"
namespace Mesh.Util;

public class MathHelper
{
    public int Add(int a, int b)
    {
        return a + b;
    }
}
"#;
        let mut p = parser();
        let nodes = CSharpExtractor::extract(Path::new("MathHelper.cs"), code, 0, &mut p);
        let add = nodes.iter().find(|n| n.name == "Add").expect("method");
        assert_eq!(add.kind, NodeKind::ServiceClass);
    }
}
