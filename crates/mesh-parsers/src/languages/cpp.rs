use crate::languages::FileIndex;
use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};

pub struct CppExtractor;

impl CppExtractor {
    /// Test/ad-hoc entry point: parses `content` itself. Production indexing goes
    /// through [`Self::extract_file_index`] via `PolyglotIndexer`, which parses once
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
        Self::extract_file_index(file_path, content, repo_id, &tree).nodes
    }

    /// Same extraction as [`Self::extract`], plus quoted `#include` dependencies
    /// (Item 2: `find_dependents` for C++). Angle-bracket includes (`<vector>`) are
    /// system headers and are never treated as dependencies.
    pub fn extract_file_index(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        tree: &Tree,
    ) -> FileIndex {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();
        let root = tree.root_node();
        let source_bytes = content.as_bytes();
        let mut package_name = mesh_core::detect_service_package(&file_path, None);

        let mut grpc_services = Vec::new();
        Self::collect_grpc_service_names(root, source_bytes, &mut grpc_services, 0);

        Self::visit_node(
            root,
            source_bytes,
            &file_path,
            repo_id,
            &mut package_name,
            &grpc_services,
            &mut nodes,
            0,
        );

        let mut dependencies = Vec::new();
        let includes = Self::quoted_local_includes(root, source_bytes);
        if !includes.is_empty() {
            let content_lines: Vec<&str> = content.lines().collect();
            for (i, node) in nodes.iter().enumerate() {
                let body = content_lines
                    .get(node.line_start.saturating_sub(1)..node.line_end)
                    .unwrap_or(&[]);
                for target in &includes {
                    // Reuse the "is it actually used in this node's line range?"
                    // heuristic (see languages/mod.rs TS import handling) so a
                    // file-level include does not attach to every symbol.
                    let is_used = body.iter().any(|l| l.contains(target.as_str()));
                    if is_used {
                        dependencies.push((i, target.clone()));
                    }
                }
            }
        }

        FileIndex {
            nodes,
            dependencies,
            ..Default::default()
        }
    }

    /// Collects the file-stem of every quoted `#include "..."` in the tree.
    /// Angle-bracket includes (`#include <vector>`, `system_lib_string`) are
    /// system headers and are deliberately excluded.
    fn quoted_local_includes(root: Node, source: &[u8]) -> Vec<CompactStr> {
        let mut out = Vec::new();
        Self::collect_includes(root, source, &mut out, 0);
        out
    }

    fn collect_includes(node: Node, source: &[u8], out: &mut Vec<CompactStr>, depth: usize) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if node.kind() == "preproc_include" {
            if let Some(path_node) = node.child_by_field_name("path") {
                if path_node.kind() == "string_literal" {
                    if let Ok(text) = path_node.utf8_text(source) {
                        let trimmed = text.trim_matches('"');
                        let stem = Path::new(trimmed)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(trimmed);
                        out.push(CompactStr::new(stem));
                    }
                } else if path_node.kind() == "system_lib_string" {
                    if let Ok(text) = path_node.utf8_text(source) {
                        let inner = text.trim_matches('<').trim_matches('>');
                        // Intra-project headers formatted in Google/CMake style (e.g. <billing/service.h>
                        // or <core/types.hpp>) contain directory separators or non-standard C++ extensions.
                        // Standard library headers like <vector>, <string>, <iostream> are excluded.
                        if inner.contains('/')
                            || inner.ends_with(".hpp")
                            || inner.ends_with(".hh")
                            || inner.ends_with(".hxx")
                        {
                            let stem = Path::new(inner)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(inner);
                            out.push(CompactStr::new(stem));
                        }
                    }
                }
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_includes(child, source, out, depth + 1);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package_name: &mut CompactStr,
        grpc_services: &[CompactStr],
        nodes: &mut Vec<ContractNode>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }

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
                        let kind = if Self::has_grpc_service_base(node, source) {
                            NodeKind::GrpcService
                        } else if Self::is_pure_interface(node, source) {
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

                    // Out-of-line methods on a `::grpc::Service` subclass (`Foo::SayHello`,
                    // where `Foo` derives from `::grpc::Service`) are RPC handlers.
                    let is_grpc_method = Self::declarator_qualifier(node, source)
                        .is_some_and(|q| grpc_services.iter().any(|s| s.as_str() == q));

                    let kind = if is_grpc_method {
                        NodeKind::GrpcMethod
                    } else if func_name.starts_with("Handle")
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
            Self::visit_node(
                child,
                source,
                file_path,
                repo_id,
                package_name,
                grpc_services,
                nodes,
                depth + 1,
            );
        }
    }

    /// True if `node` (a `class_specifier`/`struct_specifier`) directly extends
    /// `::grpc::Service` (or the generated `Service` base), per Item 5.
    fn has_grpc_service_base(node: Node, source: &[u8]) -> bool {
        let mut cursor = node.walk();
        let found = node.children(&mut cursor).any(|child| {
            child.kind() == "base_class_clause"
                && child
                    .utf8_text(source)
                    .is_ok_and(|text| text.contains("grpc::Service"))
        });
        found
    }

    /// Collects the names of every class/struct in the tree that derives from
    /// `::grpc::Service`, so out-of-line method definitions (`Foo::Method`) can be
    /// attributed back to their owning gRPC service.
    fn collect_grpc_service_names(
        node: Node,
        source: &[u8],
        out: &mut Vec<CompactStr>,
        depth: usize,
    ) {
        if depth > crate::guard::AstGuard::MAX_NESTING_DEPTH {
            return;
        }
        if matches!(node.kind(), "class_specifier" | "struct_specifier")
            && node.child_by_field_name("body").is_some()
            && Self::has_grpc_service_base(node, source)
        {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
            {
                out.push(CompactStr::new(name));
            }
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            Self::collect_grpc_service_names(child, source, out, depth + 1);
        }
    }

    /// The scope of a `Foo::Method` qualified declarator (`Foo`), if any.
    fn declarator_qualifier<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
        let mut current = node.child_by_field_name("declarator")?;
        loop {
            match current.kind() {
                "qualified_identifier" => {
                    return current.child_by_field_name("scope")?.utf8_text(source).ok();
                }
                "function_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "parenthesized_declarator" => {
                    current = current
                        .child_by_field_name("declarator")
                        .or_else(|| current.named_child(0))?;
                }
                "template_function" => {
                    current = current.child_by_field_name("name")?;
                }
                _ => return None,
            }
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

    // --- Item 5: gRPC extractor parity -------------------------------------------------

    #[test]
    fn test_cpp_extractor_grpc_service_and_method() {
        let code = r#"
class GreeterService final : public ::grpc::Service {
public:
    grpc::Status SayHello(grpc::ServerContext* context) override;
};

grpc::Status GreeterService::SayHello(grpc::ServerContext* context) {
    return grpc::Status::OK;
}

class PlainWidget {
public:
    void Render() {}
};
"#;
        let mut p = parser();
        let nodes = CppExtractor::extract(Path::new("greeter.cpp"), code, 1, &mut p);
        let find = |name: &str| nodes.iter().find(|n| n.name == name);

        assert_eq!(
            find("GreeterService").expect("grpc service").kind,
            NodeKind::GrpcService
        );
        assert_eq!(
            find("SayHello").expect("grpc method").kind,
            NodeKind::GrpcMethod
        );
        // A plain class unrelated to ::grpc::Service is unaffected.
        assert_eq!(find("PlainWidget").unwrap().kind, NodeKind::ServiceClass);
        assert_eq!(find("Render").unwrap().kind, NodeKind::ServiceClass);
    }

    // --- Item 2: quoted #include dependencies -------------------------------------------

    #[test]
    fn test_cpp_extractor_excludes_angle_bracket_system_includes() {
        let code = r#"
#include <vector>
#include "Shape.hpp"

class Circle : public Shape {
public:
    double Area() const { return 3.14; }
};
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let index = CppExtractor::extract_file_index(Path::new("circle.cpp"), code, 0, &tree);

        // "vector" (system, angle-bracket) must never appear as a dependency target.
        assert!(
            index
                .dependencies
                .iter()
                .all(|(_, t)| t.as_str() != "vector"),
            "angle-bracket system include must not be treated as a dependency"
        );
        // "Shape" (quoted, local) is used by `Circle` and must be attached.
        assert!(
            index
                .dependencies
                .iter()
                .any(|(_, t)| t.as_str() == "Shape"),
            "quoted local include used by a node must be a dependency"
        );
    }

    #[test]
    fn test_cpp_include_dependency_resolves_via_find_dependents() {
        // File 1: declares `Shape`.
        let header_code = r#"
class Shape {
public:
    virtual ~Shape() = default;
    virtual double Area() const = 0;
};
"#;
        // File 2: includes the local header and uses `Shape` as a base class.
        let consumer_code = r#"
#include "Shape.hpp"
#include <vector>

class Circle : public Shape {
public:
    double Area() const override { return 3.14; }
};
"#;
        let mut p = parser();
        let header_tree = p.parse(header_code, None).expect("parse");
        let header_index =
            CppExtractor::extract_file_index(Path::new("Shape.hpp"), header_code, 0, &header_tree);
        let mut p2 = parser();
        let consumer_tree = p2.parse(consumer_code, None).expect("parse");
        let consumer_index = CppExtractor::extract_file_index(
            Path::new("circle.cpp"),
            consumer_code,
            0,
            &consumer_tree,
        );

        let mut merged = FileIndex::default();
        merged.merge(header_index);
        merged.merge(consumer_index);

        let mut graph = mesh_core::ContractGraph::new();
        merged.apply(&mut graph);

        let dependents = graph.find_dependents("Shape");
        assert!(
            dependents.iter().any(|n| n.name == "Circle"),
            "Circle should be a dependent of Shape via the quoted #include"
        );
    }

    #[test]
    fn test_cpp_intra_project_angle_bracket_include() {
        let code = r#"
#include <vector>
#include <billing/service.h>
#include <auth/jwt.hpp>

class BillingClient {
public:
    service::BillingService svc;
    jwt::Token tok;
    void Pay() {}
};
"#;
        let mut p = parser();
        let tree = p.parse(code, None).expect("parse");
        let index = CppExtractor::extract_file_index(Path::new("client.cpp"), code, 0, &tree);
        assert!(
            index
                .dependencies
                .iter()
                .all(|(_, t)| t.as_str() != "vector"),
            "vector must be excluded"
        );
        assert!(
            index
                .dependencies
                .iter()
                .any(|(_, t)| t.as_str() == "service"),
            "service.h must be included"
        );
        assert!(
            index.dependencies.iter().any(|(_, t)| t.as_str() == "jwt"),
            "jwt.hpp must be included"
        );
    }
}
