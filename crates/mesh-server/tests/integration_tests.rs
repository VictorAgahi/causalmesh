#![allow(clippy::unwrap_used, clippy::expect_used)]

use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config, ContractNode, NodeKind,
};
use mesh_server::tools::ToolRegistry;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;

fn setup_test_environment() -> (Arc<AppState>, tempfile::TempDir) {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let base_path = dunce::canonicalize(temp_dir.path()).expect("canonicalize");

    // Create a mock service structure
    let auth_dir = base_path.join("services").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("create auth dir");

    let mut auth_file = File::create(auth_dir.join("AuthController.java")).expect("create file");
    writeln!(
        auth_file,
        r#"package com.mesh.auth;

@RestController
public class AuthController {{
    @PostMapping("/login")
    public String login() {{
        return "token";
    }}
}}
"#
    )
    .expect("write file");

    let proto_dir = base_path.join("proto-registry");
    std::fs::create_dir_all(&proto_dir).expect("create proto dir");
    let mut proto_file = File::create(proto_dir.join("auth.proto")).expect("create file");
    writeln!(
        proto_file,
        r#"syntax = "proto3";
package auth.v1;
service AuthService {{
    rpc AuthenticateUser (AuthRequest) returns (AuthResponse);
}}
"#
    )
    .expect("write proto");

    let cfg_str = r#"
[workspace]
name = "test-mesh"
version = "2.9.0"
roots = ["./services/*", "./proto-registry"]

[engines.policy.stop_rules]
"proto-registry" = "STOP CASCADE CI"
"#;

    let config = Config::load_from_str(cfg_str).expect("parse config");
    let allowed_roots = expand_roots(&config.workspace.roots, &base_path).expect("expand roots");

    let log_file = base_path.join("audit.log");
    let audit = Arc::new(AuditLogger::new(Some(log_file)).expect("audit logger"));
    let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan engine"));

    let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

    // Seed contract graph
    let mut graph = (*state.contract_graph.load().as_ref()).clone();
    let node_id = graph.add_node(ContractNode {
        id: 0,
        name: "AuthController".into(),
        kind: NodeKind::ServiceClass,
        file_path: auth_dir.join("AuthController.java"),
        line_start: 3,
        line_end: 10,
        package: "com.mesh.auth".into(),
        repo_id: 1,
        signature: Some("public class AuthController".into()),
        docstring: None,
    });
    graph.add_dependency(node_id, "UserAuthRequest");
    state.contract_graph.store(Arc::new(graph));

    (state, temp_dir)
}

#[tokio::test]
async fn test_smart_search_success() {
    let (state, _temp) = setup_test_environment();
    let roots = state.allowed_roots.load();
    let auth_root = roots
        .iter()
        .find(|r| r.to_string_lossy().contains("auth"))
        .expect("find auth root");

    let args = json!({
        "query": "AuthController",
        "scope": auth_root.to_string_lossy(),
        "_meta": {
            "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        }
    });

    let res = ToolRegistry::call_tool("smart_search", args, state.clone()).await;
    assert!(res.is_ok());

    let val = res.unwrap();
    let text = val["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Search Results for `AuthController`"));
    assert!(text.contains("AuthController.java"));
}

#[tokio::test]
async fn test_smart_search_sandbox_escape_rejected() {
    let (state, _temp) = setup_test_environment();

    // Attempt sandbox breakout to /etc or /tmp parent
    let args = json!({
        "query": "passwd",
        "scope": "/etc"
    });

    let res = ToolRegistry::call_tool("smart_search", args, state).await;
    assert!(res.is_err());
    let (code, msg) = res.unwrap_err();
    assert_eq!(code, -32602); // RFC Commandment 4
    assert!(msg.contains("Sandbox escape") || msg.contains("Path not found"));
}

#[tokio::test]
async fn test_find_dependents_success() {
    let (state, _temp) = setup_test_environment();

    let args = json!({
        "target": "UserAuthRequest"
    });

    let res = ToolRegistry::call_tool("find_dependents", args, state).await;
    assert!(res.is_ok());
    let val = res.unwrap();
    let text = val["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("In-Memory Reverse Dependency Graph for `UserAuthRequest`"));
    assert!(text.contains("AuthController"));
}

#[tokio::test]
async fn test_governance_rsah_trigger() {
    let (state, _temp) = setup_test_environment();
    let roots = state.allowed_roots.load();
    let proto_root = roots
        .iter()
        .find(|r| r.to_string_lossy().contains("proto-registry"))
        .expect("find proto root");

    let args = json!({
        "query": "AuthService",
        "scope": proto_root.to_string_lossy()
    });

    let res = ToolRegistry::call_tool("smart_search", args, state).await;
    assert!(res.is_ok());
    let val = res.unwrap();
    let text = val["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("GOVERNANCE_BLOCKED"));
    assert!(text.contains("CONTRACT_FIRST_CASCADE_CI"));
    assert!(text.contains("STOP_AND_REPORT_TO_USER"));
}
