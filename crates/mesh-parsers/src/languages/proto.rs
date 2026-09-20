use mesh_core::{CompactStr, ContractNode, NodeKind, RepoId};
use std::path::Path;

pub struct ProtoExtractor;

impl ProtoExtractor {
    pub fn extract(file_path: &Path, content: &str, repo_id: RepoId) -> Vec<ContractNode> {
        let mut nodes = Vec::new();
        let mut current_package = CompactStr::default();
        let mut in_service = None::<CompactStr>;

        // Normalize statements across newlines, semicolons, and braces to handle all formatting
        let normalized = content
            .replace(';', ";\n")
            .replace('{', "{\n")
            .replace('}', "\n}\n");

        for (idx, line) in normalized.lines().enumerate() {
            let line_num = idx + 1;
            let trimmed = line.trim();

            if trimmed.starts_with("//") || trimmed.is_empty() {
                continue;
            }

            // package foo.bar;
            if trimmed.starts_with("package ") {
                let pkg = trimmed
                    .trim_start_matches("package ")
                    .trim_end_matches(';')
                    .trim();
                current_package = CompactStr::new(pkg);
                continue;
            }

            // service UserService {
            if trimmed.starts_with("service ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    let s_name = parts[1].trim_end_matches('{').trim();
                    let s_name_compact = CompactStr::new(s_name);
                    in_service = Some(s_name_compact.clone());

                    nodes.push(ContractNode {
                        id: 0,
                        name: s_name_compact,
                        kind: NodeKind::GrpcService,
                        file_path: file_path.to_path_buf(),
                        line_start: line_num,
                        line_end: line_num,
                        package: current_package.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(trimmed)),
                        docstring: None,
                    });
                }
                continue;
            }

            // rpc MethodName (Req) returns (Resp);
            if trimmed.starts_with("rpc ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    let rpc_name = parts[1].trim_end_matches('(').trim();
                    let full_service = in_service
                        .as_ref()
                        .map(|s| s.as_str())
                        .unwrap_or("UnknownService");
                    let fqcn_name = format!("{full_service}.{rpc_name}");

                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(&fqcn_name),
                        kind: NodeKind::GrpcMethod,
                        file_path: file_path.to_path_buf(),
                        line_start: line_num,
                        line_end: line_num,
                        package: current_package.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(trimmed)),
                        docstring: None,
                    });
                }
                continue;
            }

            // message MessageName {
            if trimmed.starts_with("message ") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 2 {
                    let msg_name = parts[1].trim_end_matches('{').trim();
                    nodes.push(ContractNode {
                        id: 0,
                        name: CompactStr::new(msg_name),
                        kind: NodeKind::ProtoMessage,
                        file_path: file_path.to_path_buf(),
                        line_start: line_num,
                        line_end: line_num,
                        package: current_package.clone(),
                        repo_id,
                        signature: Some(CompactStr::new(trimmed)),
                        docstring: None,
                    });
                }
                continue;
            }

            if trimmed == "}" {
                in_service = None;
            }
        }

        nodes
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
    }
}
