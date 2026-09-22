use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Parser};

pub struct TypeScriptExtractor;

/// Historical hardcoded gRPC decorator, used when `controller_annotations`
/// is not configured.
const DEFAULT_GRPC_ANNOTATION: &str = "@GrpcMethod";

/// Per-file invariants threaded through the recursive `visit_node` walk,
/// bundled to keep its argument count down.
struct VisitCtx<'a> {
    file_path: &'a FilePath,
    repo_id: RepoId,
    package_name: &'a CompactStr,
    grpc_annotations: &'a [String],
}

impl TypeScriptExtractor {
    /// Extracts using the legacy hardcoded `@GrpcMethod` decorator.
    pub fn extract(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
    ) -> Vec<ContractNode> {
        Self::extract_with_config(
            file_path,
            content,
            repo_id,
            parser,
            imports,
            &[DEFAULT_GRPC_ANNOTATION.to_string()],
        )
    }

    /// Same as [`Self::extract`], but `grpc_annotations` (from
    /// `[engines.contracts.grpc] controller_annotations`) replaces the
    /// hardcoded `@GrpcMethod` decorator list used to recognise gRPC handlers.
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        parser: &mut Parser,
        imports: &mut Vec<(String, String)>, // (consumer_symbol, imported_package_or_symbol)
        grpc_annotations: &[String],
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
        let ctx = VisitCtx {
            file_path: &file_path,
            repo_id,
            package_name: &package_name,
            grpc_annotations,
        };

        Self::visit_node(root, source_bytes, &ctx, &mut nodes, imports);
        nodes
    }

    fn visit_node(
        node: Node,
        source: &[u8],
        ctx: &VisitCtx,
        nodes: &mut Vec<ContractNode>,
        imports: &mut Vec<(String, String)>,
    ) {
        let file_path = ctx.file_path;
        let repo_id = ctx.repo_id;
        let package_name = ctx.package_name;
        let grpc_annotations = ctx.grpc_annotations;
        match node.kind() {
            "import_statement" => {
                if let Ok(text) = node.utf8_text(source) {
                    if let Some(from_idx) = text.find("from") {
                        let from_str = text[from_idx + 4..]
                            .trim()
                            .trim_matches(';')
                            .trim()
                            .trim_matches('\'')
                            .trim_matches('"');

                        imports.push((String::new(), from_str.to_string()));

                        for named in Self::collect_named_import_specifiers(node, source) {
                            imports.push((String::new(), named));
                        }
                    }
                }
            }
            "class_declaration" | "interface_declaration" => {
                let class_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("UnknownClass");

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("class {class_name}"));

                let kind = if node.kind() == "interface_declaration" {
                    NodeKind::Interface
                } else {
                    NodeKind::ServiceClass
                };

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
            "method_definition" => {
                let method_name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("unknownMethod");

                let mut kind = NodeKind::ServiceClass;
                let mut grpc_target = None;

                let mut full_text = String::new();
                if let Some(prev) = node.prev_sibling() {
                    if prev.kind() == "decorator" {
                        if let Ok(t) = prev.utf8_text(source) {
                            full_text.push_str(t);
                            full_text.push(' ');
                        }
                    }
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "decorator" {
                        if let Ok(t) = child.utf8_text(source) {
                            full_text.push_str(t);
                            full_text.push(' ');
                        }
                    }
                }
                if let Ok(t) = node.utf8_text(source) {
                    full_text.push_str(t);
                }

                let matched_annotation = grpc_annotations
                    .iter()
                    .find(|a| full_text.contains(a.as_str()));
                if let Some(annotation) = matched_annotation {
                    kind = NodeKind::GrpcMethod;
                    grpc_target = Self::extract_grpc_method(&full_text, annotation);
                } else if full_text.contains("@Get")
                    || full_text.contains("@Post")
                    || full_text.contains("@Put")
                {
                    kind = NodeKind::HttpEndpoint;
                }

                let first_line = node
                    .utf8_text(source)
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .unwrap_or_else(|| format!("{method_name}()"));

                let final_name = if let Some(g) = grpc_target {
                    CompactStr::new(g)
                } else {
                    CompactStr::new(method_name)
                };

                nodes.push(ContractNode {
                    id: 0,
                    name: final_name,
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
            Self::visit_node(child, source, ctx, nodes, imports);
        }
    }

    /// Walks an `import_statement` node's `import_clause` -> `named_imports`
    /// children to collect each individually named specifier's *original*
    /// exported identifier (the `name` field — never the local `alias`,
    /// since matching needs the name as declared in the source file, not
    /// however the importer chose to rebind it locally).
    fn collect_named_import_specifiers<'a>(node: Node<'a>, source: &'a [u8]) -> Vec<String> {
        let mut specifiers = Vec::new();

        for clause in node.children(&mut node.walk()) {
            if clause.kind() != "import_clause" {
                continue;
            }
            for part in clause.children(&mut clause.walk()) {
                if part.kind() != "named_imports" {
                    continue;
                }
                for spec in part.children(&mut part.walk()) {
                    if spec.kind() != "import_specifier" {
                        continue;
                    }
                    if let Some(name_node) = spec.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(source) {
                            specifiers.push(name.to_string());
                        }
                    }
                }
            }
        }

        specifiers
    }

    fn extract_grpc_method(text: &str, annotation: &str) -> Option<String> {
        if let Some(idx) = text.find(annotation) {
            let rest = &text[idx + annotation.len()..];
            if let Some(paren_open) = rest.find('(') {
                if let Some(paren_close) = rest[paren_open + 1..].find(')') {
                    let args = &rest[paren_open + 1..paren_open + 1 + paren_close];
                    let parts: Vec<&str> = args
                        .split(',')
                        .map(|s| s.trim().trim_matches('\'').trim_matches('"'))
                        .collect();
                    if parts.len() == 2 {
                        return Some(format!("{}.{}", parts[0], parts[1]));
                    } else if let Some(first) = parts.first() {
                        return Some(first.to_string());
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ts_extractor() {
        let code = r#"
import { UserAuthRequest } from '@volontariapp/domain-user';

@Controller('auth')
export class AuthController {
    @GrpcMethod('AuthService', 'AuthenticateUser')
    async authenticateUser(data: any): Promise<any> {
        return null;
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();

        let mut imports = Vec::new();
        let nodes = TypeScriptExtractor::extract(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports,
        );

        // One module-path-level entry plus one entry per named specifier
        // (here just `UserAuthRequest`).
        assert_eq!(imports.len(), 2);
        assert!(imports
            .iter()
            .any(|(_, t)| t == "@volontariapp/domain-user"));
        assert!(imports.iter().any(|(_, t)| t == "UserAuthRequest"));
        assert!(nodes.iter().any(|n| n.name == "AuthController"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser" && n.kind == NodeKind::GrpcMethod));
    }

    #[test]
    fn test_ts_extractor_configurable_controller_annotations() {
        let code = r#"
@Controller('auth')
export class AuthController {
    @RpcHandler('AuthService', 'AuthenticateUser')
    async authenticateUser(data: any): Promise<any> {
        return null;
    }
}
"#;
        let mut parser = Parser::new();
        let lang = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        parser.set_language(&lang).unwrap();
        let mut imports = Vec::new();

        // Default decorator list (`@GrpcMethod`) does not recognise `@RpcHandler`.
        let default_nodes = TypeScriptExtractor::extract(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports,
        );
        assert!(!default_nodes
            .iter()
            .any(|n| n.kind == NodeKind::GrpcMethod));

        // A configured `controller_annotations` list must produce an
        // observably different result: the method is now recognised as a
        // gRPC handler and projected via the same `Service.Method` parsing.
        let mut imports2 = Vec::new();
        let configured_nodes = TypeScriptExtractor::extract_with_config(
            Path::new("auth.controller.ts"),
            code,
            4,
            &mut parser,
            &mut imports2,
            &["@RpcHandler".to_string()],
        );
        assert!(configured_nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser" && n.kind == NodeKind::GrpcMethod));
    }
}
