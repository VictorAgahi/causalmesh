use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct CppExtractor;

impl CppExtractor {
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
            "namespace_definition" => {
                // The first named namespace becomes the package; nested ones are scoped below it.
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    *package_name = mesh_core::detect_service_package(file_path, Some(name));
                }
            }
            "class_specifier" | "struct_specifier" => {
                // Only declarations with a body are definitions; forward decls are skipped.
                if node.child_by_field_name("body").is_some() {
                    if let Some(name) = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source).ok())
                    {
                        let first_line = Self::first_line(node, source, name);
                        let kind = if Self::is_pure_interface(node, source) {
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
                            signature: Some(CompactStr::new(first_line)),
                            docstring: None,
                        });
                    }
                }
            }
            "function_definition" => {
                if let Some(func_name) =
                    Self::declarator_name(node, source).filter(|n| !n.starts_with('~'))
                {
                    let first_line = Self::first_line(node, source, func_name);

                    let kind = if func_name.starts_with("Handle")
                        || func_name.starts_with("handle")
                        || func_name.ends_with("Handler")
                    {
                        NodeKind::HttpEndpoint
                    } else {
                        NodeKind::ServiceClass
                    };

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
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::visit_node(child, source, file_path, repo_id, package_name, nodes);
        }
    }

    /// Walks a `function_definition` declarator chain down to the identifier, skipping
    /// pointer/reference/qualified wrappers (`Foo::bar`, `*fn`, `operator()`).
    fn declarator_name<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
        let mut current = node.child_by_field_name("declarator")?;
        loop {
            match current.kind() {
                "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
                    return current.utf8_text(source).ok();
                }
                "qualified_identifier" => {
                    // `Service::Method` — keep the trailing segment as the symbol name
                    current = current.child_by_field_name("name")?;
                }
                "function_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "parenthesized_declarator" => {
                    current = current.child_by_field_name("declarator").or_else(|| {
                        // reference_declarator has no `declarator` field; take the first named child
                        current.named_child(0)
                    })?;
                }
                "template_function" => {
                    current = current.child_by_field_name("name")?;
                }
                _ => return None,
            }
        }
    }

    /// A class whose every method is pure virtual (`= 0`) is treated as an interface.
    /// Defaulted/deleted special members (`= default`, `= delete`) are ignored.
    fn is_pure_interface(node: Node, _source: &[u8]) -> bool {
        let body = match node.child_by_field_name("body") {
            Some(b) => b,
            None => return false,
        };
        let mut saw_method = false;
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "field_declaration" => {
                    let is_method = child
                        .child_by_field_name("declarator")
                        .is_some_and(|d| d.kind() == "function_declarator");
                    if is_method {
                        saw_method = true;
                        // Pure virtual methods carry `default_value: (number_literal)` for the `= 0`.
                        if child.child_by_field_name("default_value").is_none() {
                            return false;
                        }
                    }
                }
                "function_definition" => {
                    let mut inner = child.walk();
                    let is_special = child.children(&mut inner).any(|c| {
                        c.kind() == "default_method_clause" || c.kind() == "delete_method_clause"
                    });
                    if !is_special {
                        return false;
                    }
                }
                _ => {}
            }
        }
        saw_method
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
        let lang = tree_sitter_cpp::LANGUAGE.into();
        parser.set_language(&lang).unwrap();
        parser
    }

    #[test]
    fn test_cpp_extractor_classes_and_functions() {
        let code = r#"
#include <string>

namespace auth {

class IAuthService {
public:
    virtual ~IAuthService() = default;
    virtual bool Authenticate(const std::string& token) = 0;
};

struct Credentials {
    std::string user;
};

class AuthService : public IAuthService {
public:
    bool Authenticate(const std::string& token) override {
        return !token.empty();
    }
};

bool AuthService::Validate(const Credentials& c) {
    return !c.user.empty();
}

void HandleLogin(int fd) {}

} // namespace auth
"#;
        let mut p = parser();
        let nodes = CppExtractor::extract(Path::new("services/auth/auth.cpp"), code, 3, &mut p);

        let find = |name: &str| nodes.iter().find(|n| n.name == name);

        let iface = find("IAuthService").expect("interface");
        assert_eq!(iface.kind, NodeKind::Interface);
        assert_eq!(find("Credentials").unwrap().kind, NodeKind::ServiceClass);
        assert_eq!(find("AuthService").unwrap().kind, NodeKind::ServiceClass);
        assert!(find("Authenticate").is_some());
        assert!(find("Validate").is_some(), "qualified out-of-class method");
        assert_eq!(find("HandleLogin").unwrap().kind, NodeKind::HttpEndpoint);
        // Package resolved from services/<name>/ directory takes precedence over namespace
        assert_eq!(find("AuthService").unwrap().package.as_str(), "auth");
        assert!(nodes.iter().all(|n| n.repo_id == 3));
    }

    #[test]
    fn test_cpp_extractor_skips_forward_declarations() {
        let code = "class Forward;\nstruct Opaque;\n";
        let mut p = parser();
        let nodes = CppExtractor::extract(Path::new("fwd.hpp"), code, 0, &mut p);
        assert!(nodes.is_empty());
    }
}
