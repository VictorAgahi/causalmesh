use crate::decapitate::LanguageKind;
use crate::guard::AstGuard;
use mesh_core::{CompactStr, ContractNode, FilePath, NodeKind, RepoId};
use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Node, Tree};

pub struct ProtoExtractor;

impl ProtoExtractor {
    /// Extracts with the canonical projection: RPC nodes are named `Service.Method`.
    pub fn extract(file_path: &Path, content: &str, repo_id: RepoId) -> Vec<ContractNode> {
        Self::extract_with_config(file_path, content, repo_id, true)
    }

    /// Extracts with configurable RPC method projection:
    /// `canonical_fqcn_projection = true` yields `Service.Method`, `false` yields bare method name.
    /// Test/ad-hoc entry point: parses `content` itself, at the query-time budget.
    /// Production indexing goes through [`Self::extract_with_parser`] via
    /// `PolyglotIndexer`, which parses once with `AstGuard::parse_with` at the (much
    /// larger) indexing budget, so a parse failure is visible instead of silently
    /// producing an empty result indistinguishable from a legitimately empty file.
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        canonical_fqcn_projection: bool,
    ) -> Vec<ContractNode> {
        AstGuard::with_parser(LanguageKind::Protobuf, |parser| {
            parser
                .parse(content, None)
                .map(|tree| {
                    Self::extract_with_parser(
                        file_path,
                        content,
                        repo_id,
                        canonical_fqcn_projection,
                        &tree,
                    )
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Extracts protobuf contract nodes from an already-parsed tree.
    pub fn extract_with_parser(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        canonical_fqcn_projection: bool,
        tree: &Tree,
    ) -> Vec<ContractNode> {
        let file_path: FilePath = Arc::from(file_path);
        let mut nodes = Vec::new();

        let source = content.as_bytes();
        let root = tree.root_node();
        let mut current_package = CompactStr::default();

        // 1. Locate top-level package declaration if present
        for i in 0..root.child_count() {
            if let Some(child) = root.child(i) {
                if child.kind() == "package" {
                    if let Some(pkg) = Self::extract_package_name(child, source) {
                        current_package = pkg;
                        break;
                    }
                }
            }
        }

        // 2. Walk services, RPC methods, and messages
        Self::walk_scope(
            root,
            source,
            &file_path,
            repo_id,
            &current_package,
            canonical_fqcn_projection,
            &mut nodes,
        );

        nodes
    }

    fn extract_package_name(package_node: Node, source: &[u8]) -> Option<CompactStr> {
        for i in 0..package_node.child_count() {
            if let Some(child) = package_node.child(i) {
                if child.kind() == "full_ident" || child.kind() == "identifier" {
                    if let Ok(text) = child.utf8_text(source) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            return Some(CompactStr::new(trimmed));
                        }
                    }
                }
            }
        }

        if let Ok(raw) = package_node.utf8_text(source) {
            let pkg = raw
                .trim_start_matches("package")
                .trim_end_matches(';')
                .trim();
            if !pkg.is_empty() {
                return Some(CompactStr::new(pkg));
            }
        }

        None
    }

    fn walk_scope(
        parent: Node,
        source: &[u8],
        file_path: &FilePath,
        repo_id: RepoId,
        package: &CompactStr,
        canonical_fqcn_projection: bool,
        nodes: &mut Vec<ContractNode>,
    ) {
        for i in 0..parent.child_count() {
            let child = match parent.child(i) {
                Some(c) => c,
                None => continue,
            };

            match child.kind() {
                "service" => {
                    let service_name = match Self::find_child_text(child, "service_name", source) {
                        Some(name) => name,
                        None => continue,
                    };

                    let line_start = child.start_position().row + 1;
                    let line_end = child.end_position().row + 1;
                    let signature = Self::extract_signature(child, source);
                    let docstring = Self::extract_preceding_docstring(child, source);

                    nodes.push(ContractNode {
                        id: 0,
                        name: service_name.clone(),
                        kind: NodeKind::GrpcService,
                        file_path: file_path.clone(),
                        line_start,
                        line_end,
                        package: package.clone(),
                        repo_id,
                        signature,
                        docstring,
                    });

                    // Walk RPC declarations within service body
                    for j in 0..child.child_count() {
                        if let Some(rpc_child) = child.child(j) {
                            if rpc_child.kind() == "rpc" {
                                if let Some(rpc_name) =
                                    Self::find_child_text(rpc_child, "rpc_name", source)
                                {
                                    let full_name = if canonical_fqcn_projection {
                                        format!("{service_name}.{rpc_name}")
                                    } else {
                                        rpc_name.to_string()
                                    };

                                    let rpc_line_start = rpc_child.start_position().row + 1;
                                    let rpc_line_end = rpc_child.end_position().row + 1;
                                    let rpc_sig = Self::extract_signature(rpc_child, source);
                                    let rpc_doc =
                                        Self::extract_preceding_docstring(rpc_child, source);

                                    nodes.push(ContractNode {
                                        id: 0,
                                        name: CompactStr::new(&full_name),
                                        kind: NodeKind::GrpcMethod,
                                        file_path: file_path.clone(),
                                        line_start: rpc_line_start,
                                        line_end: rpc_line_end,
                                        package: package.clone(),
                                        repo_id,
                                        signature: rpc_sig,
                                        docstring: rpc_doc,
                                    });
                                }
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(message_name) = Self::find_child_text(child, "message_name", source)
                    {
                        let line_start = child.start_position().row + 1;
                        let line_end = child.end_position().row + 1;
                        let signature = Self::extract_signature(child, source);
                        let docstring = Self::extract_preceding_docstring(child, source);

                        nodes.push(ContractNode {
                            id: 0,
                            name: message_name,
                            kind: NodeKind::ProtoMessage,
                            file_path: file_path.clone(),
                            line_start,
                            line_end,
                            package: package.clone(),
                            repo_id,
                            signature,
                            docstring,
                        });

                        // Recursively walk message_body for nested messages
                        if let Some(body) = Self::find_child_by_kind(child, "message_body") {
                            Self::walk_scope(
                                body,
                                source,
                                file_path,
                                repo_id,
                                package,
                                canonical_fqcn_projection,
                                nodes,
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn find_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == kind {
                    return Some(child);
                }
            }
        }
        None
    }

    fn find_child_text(parent: Node, child_kind: &str, source: &[u8]) -> Option<CompactStr> {
        let child = Self::find_child_by_kind(parent, child_kind)?;
        let text = child.utf8_text(source).ok()?.trim();
        if !text.is_empty() {
            Some(CompactStr::new(text))
        } else {
            None
        }
    }

    fn extract_signature(node: Node, source: &[u8]) -> Option<CompactStr> {
        let text = node.utf8_text(source).ok()?;
        let first_line = text.lines().next()?.trim();
        if !first_line.is_empty() {
            Some(CompactStr::new(first_line))
        } else {
            None
        }
    }

    fn extract_preceding_docstring(node: Node, source: &[u8]) -> Option<CompactStr> {
        let mut prev = node.prev_sibling()?;
        while prev.kind() == "\n" {
            prev = prev.prev_sibling()?;
        }
        if prev.kind() == "comment" {
            let text = prev.utf8_text(source).ok()?.trim();
            if !text.is_empty() {
                return Some(CompactStr::new(text));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proto_extraction() {
        let proto = r#"
syntax = "proto3";
package auth.v1;

service AuthService {
    rpc AuthenticateUser (AuthRequest) returns (AuthResponse);
}

message AuthRequest {
    string username = 1;
}
"#;
        let nodes = ProtoExtractor::extract(Path::new("proto/auth.proto"), proto, 0);
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().any(|n| n.name == "AuthService"));
        assert!(nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser"));
        assert!(nodes.iter().any(|n| n.name == "AuthRequest"));

        let svc = nodes.iter().find(|n| n.name == "AuthService").unwrap();
        assert_eq!(svc.line_start, 5);
        assert_eq!(svc.package, "auth.v1");

        let rpc = nodes
            .iter()
            .find(|n| n.name == "AuthService.AuthenticateUser")
            .unwrap();
        assert_eq!(rpc.line_start, 6);
        assert_eq!(rpc.package, "auth.v1");

        let msg = nodes.iter().find(|n| n.name == "AuthRequest").unwrap();
        assert_eq!(msg.line_start, 9);
        assert_eq!(msg.package, "auth.v1");
    }

    #[test]
    fn test_proto_extraction_non_canonical_fqcn_projection() {
        let proto = r#"
syntax = "proto3";
package auth.v1;

service AuthService {
    rpc AuthenticateUser (AuthRequest) returns (AuthResponse);
}
"#;
        let nodes =
            ProtoExtractor::extract_with_config(Path::new("proto/auth.proto"), proto, 0, false);
        assert!(nodes.iter().any(|n| n.name == "AuthenticateUser"));
        assert!(!nodes
            .iter()
            .any(|n| n.name == "AuthService.AuthenticateUser"));
    }

    #[test]
    fn test_proto_nested_options_and_braces_edge_case() {
        let proto = r#"syntax = "proto3";
package api.v1;

service FirstService {
    rpc GetItem (GetItemRequest) returns (Item) {
        option (google.api.http) = {
            get: "/v1/items/{id}"
        };
    }
}

service SecondService {
    rpc ListItems (ListItemsRequest) returns (ListItemsResponse);
}

message OuterMessage {
    message InnerMessage {
        string token = 1;
    }
    InnerMessage inner = 1;
}
"#;
        let nodes = ProtoExtractor::extract(Path::new("proto/items.proto"), proto, 0);
        assert_eq!(nodes.len(), 6);

        assert!(nodes.iter().any(|n| n.name == "FirstService"));
        assert!(nodes.iter().any(|n| n.name == "FirstService.GetItem"));
        // SecondService methods must NOT be assigned to FirstService!
        assert!(nodes.iter().any(|n| n.name == "SecondService"));
        assert!(nodes.iter().any(|n| n.name == "SecondService.ListItems"));
        assert!(!nodes.iter().any(|n| n.name == "FirstService.ListItems"));

        // Nested message support
        assert!(nodes.iter().any(|n| n.name == "OuterMessage"));
        assert!(nodes.iter().any(|n| n.name == "InnerMessage"));
    }
}
