pub mod analyze_grpc;
pub mod analyze_impact;
pub mod find_dependents;
pub mod search_docs;
pub mod smart_search;
#[cfg(feature = "test-util")]
pub mod test_slow_op;
pub mod visualize_mesh;

use crate::protocol::RequestMeta;
use analyze_grpc::AnalyzeGrpcTool;
use analyze_impact::AnalyzeImpactTool;
use find_dependents::FindDependentsTool;
use mesh_core::AppState;
use schemars::{schema_for, JsonSchema};
use search_docs::SearchDocsTool;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use smart_search::SmartSearchTool;
use std::sync::{Arc, LazyLock};
#[cfg(feature = "test-util")]
use test_slow_op::TestSlowOpTool;
use visualize_mesh::VisualizeMeshTool;

/// JSON-RPC error tuple used throughout tool dispatch.
pub type ToolError = (i32, String);

/// Implementation-defined JSON-RPC server error (per spec, the -32000..-32099
/// range) for a call refused by active governance (RSAH). Distinct from
/// -32602 (invalid params) and -32601 (unknown tool).
pub const GOVERNANCE_BLOCKED_CODE: i32 = -32001;

/// Result of one tool invocation, before audit and MCP framing.
pub struct ToolOutput {
    pub text: String,
    pub files_accessed: Vec<String>,
    pub secrets_redacted: usize,
}

impl ToolOutput {
    pub fn text(text: String) -> Self {
        Self {
            text,
            files_accessed: Vec::new(),
            secrets_redacted: 0,
        }
    }
}

/// One MCP tool. `run` is synchronous on purpose: every tool touches the disk,
/// tree-sitter or SQLite, and the registry executes it on the blocking pool so
/// the JSON-RPC event loop (and, in daemon mode, other clients) never stalls.
pub trait McpTool {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Args: DeserializeOwned + Serialize + JsonSchema + Send + 'static;

    fn meta(args: &Self::Args) -> Option<&RequestMeta>;
    fn run(args: &Self::Args, state: &AppState) -> Result<ToolOutput, ToolError>;

    /// Optional tool-specific narrowing hint added when output payload exceeds 48 KB.
    /// Default returns None (falling back to standard narrowing recommendation).
    fn truncation_hint(_args: &Self::Args, _state: &AppState) -> Option<String> {
        None
    }

    /// The scope or target this call is about, used to match
    /// `[engines.policy.skills]` keys the same way stop rules are matched.
    /// `None` means only an exact tool-name key can recommend a skill.
    fn subject(_args: &Self::Args) -> Option<&str> {
        None
    }

    /// Whether this call would mutate the target named by `subject`, as opposed
    /// to only reading it. Active governance (RSAH) is reserved for mutations —
    /// see the note in `smart_search.rs` — so `evaluate_guard` is only consulted
    /// when this returns `true`. Every MeshMCP tool today is a read-only query,
    /// so the default (and every current override) is `false`.
    fn mutates(_args: &Self::Args) -> bool {
        false
    }
}

pub struct ToolRegistry;

impl ToolRegistry {
    /// Returns the schema definition for all enterprise MCP tools.
    /// Built once: `schema_for!` walks the whole type graph and `tools/list` is
    /// called by every client on connect.
    pub fn list_tools() -> Value {
        static TOOLS: LazyLock<Value> = LazyLock::new(|| {
            json!([
                ToolRegistry::describe::<SmartSearchTool>(),
                ToolRegistry::describe::<FindDependentsTool>(),
                ToolRegistry::describe::<AnalyzeGrpcTool>(),
                ToolRegistry::describe::<AnalyzeImpactTool>(),
                ToolRegistry::describe::<SearchDocsTool>(),
                ToolRegistry::describe::<VisualizeMeshTool>(),
            ])
        });
        TOOLS.clone()
    }

    fn describe<T: McpTool>() -> Value {
        json!({
            "name": T::NAME,
            "description": T::DESCRIPTION,
            "inputSchema": schema_for!(T::Args),
        })
    }

    /// Dispatches an incoming MCP tools/call request
    pub async fn call_tool(
        name: &str,
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<Value, ToolError> {
        let text_output = match name {
            SmartSearchTool::NAME => Self::invoke::<SmartSearchTool>(arguments, state).await?,
            FindDependentsTool::NAME => {
                Self::invoke::<FindDependentsTool>(arguments, state).await?
            }
            AnalyzeGrpcTool::NAME => Self::invoke::<AnalyzeGrpcTool>(arguments, state).await?,
            AnalyzeImpactTool::NAME => Self::invoke::<AnalyzeImpactTool>(arguments, state).await?,
            SearchDocsTool::NAME => Self::invoke::<SearchDocsTool>(arguments, state).await?,
            VisualizeMeshTool::NAME => Self::invoke::<VisualizeMeshTool>(arguments, state).await?,
            #[cfg(feature = "test-util")]
            TestSlowOpTool::NAME => Self::invoke::<TestSlowOpTool>(arguments, state).await?,
            unknown => return Err((-32601, format!("Unknown tool: {unknown}"))),
        };

        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": text_output
                }
            ]
        }))
    }

    /// Footer pointing the agent at a configured skill file. The path is what the
    /// agent needs (it can read the file itself); the title, when the file starts
    /// with a `name:` frontmatter or an `# H1`, saves it a read when it is not relevant.
    fn render_skill_hint(skill_path: &str) -> String {
        let title = std::fs::read_to_string(skill_path)
            .ok()
            .and_then(|content| Self::skill_title(&content));

        match title {
            Some(t) => format!("\n---\n**Project skill for this area**: `{skill_path}` — {t}\nRead it before proposing changes here.\n"),
            None => format!("\n---\n**Project skill for this area**: `{skill_path}`\nRead it before proposing changes here.\n"),
        }
    }

    fn skill_title(content: &str) -> Option<String> {
        for line in content.lines().take(20) {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("description:") {
                let rest = rest.trim().trim_matches(['"', '\'', '>', '|']).trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
            if let Some(rest) = line.strip_prefix("# ") {
                return Some(rest.trim().to_string());
            }
        }
        None
    }

    /// Parses arguments, runs the tool body and the audit write on the blocking
    /// pool, and returns the rendered text.
    async fn invoke<T: McpTool>(
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<String, ToolError> {
        let args: T::Args = serde_json::from_value(arguments)
            .map_err(|e| (-32602, format!("Invalid arguments for {}: {e}", T::NAME)))?;

        // Active governance (RSAH): mutating calls are subject to a stop rule.
        // For read-only tools, behavior depends on `read_governance_mode`:
        // - AllowAll (default): read queries proceed without refusal or warning.
        // - AuditWarn: read queries proceed, but log a warning trace if a stop rule matched.
        // - EnforceRefusal: read queries on guarded subjects are blocked with RSAH refusal.
        let is_mutating = T::mutates(&args);
        let mode = state.governance.read_governance_mode();
        let should_check_guard = is_mutating || mode != mesh_core::ReadGovernanceMode::AllowAll;

        if should_check_guard {
            if let Some(subject) = T::subject(&args) {
                if let Some(rsah) = state.governance.evaluate_guard(subject) {
                    if is_mutating || mode == mesh_core::ReadGovernanceMode::EnforceRefusal {
                        let payload = serde_json::to_string(&rsah).unwrap_or_else(|_| {
                            "RSAH governance refusal (payload serialization failed)".to_string()
                        });
                        return Err((GOVERNANCE_BLOCKED_CODE, payload));
                    } else if mode == mesh_core::ReadGovernanceMode::AuditWarn {
                        tracing::warn!(
                            target: "mesh::security",
                            tool = T::NAME,
                            subject = subject,
                            "Read access to RSAH guarded scope '{}' detected under AuditWarn policy",
                            subject
                        );
                    }
                }
            }
        }

        tokio::task::spawn_blocking(move || {
            let mut result = T::run(&args, &state);

            // Surface the team's own playbook for this area, so the agent reads the
            // house rules before acting instead of inferring them from the code.
            if let Ok(out) = &mut result {
                if let Some(skill) = state.governance.recommend_skill(T::NAME, T::subject(&args)) {
                    out.text.push_str(&Self::render_skill_hint(skill));
                }

                // Centralized 48 KB Payload Budget Capping
                const MAX_TOOL_OUTPUT_BYTES: usize = 48 * 1024;
                if out.text.len() > MAX_TOOL_OUTPUT_BYTES {
                    let mut cut_off = MAX_TOOL_OUTPUT_BYTES - 384;
                    while !out.text.is_char_boundary(cut_off) {
                        cut_off -= 1;
                    }
                    out.text.truncate(cut_off);
                    let hint = T::truncation_hint(&args, &state).unwrap_or_else(|| {
                        "Refine scope or pass specific search targets to narrow output.".to_string()
                    });
                    out.text.push_str(&format!(
                        "\n\n> [!NOTE]\n> Output payload truncated to fit within maximum MCP output payload cap (48 KB). {hint}\n",
                    ));
                }
            }

            let (status, files, redacted) = match &result {
                Ok(out) => ("SUCCESS", out.files_accessed.clone(), out.secrets_redacted),
                Err(_) => ("ERROR", Vec::new(), 0),
            };

            // Audit is best-effort and off the executor; the SQLite write never
            // gates the response but always happens on the same blocking thread —
            // unless `[engines.policy] cryptographic_audit_trail = false` turns the
            // whole audit trail off (absent config defaults to on).
            let audit_enabled = state
                .config
                .engines
                .policy
                .as_ref()
                .is_none_or(|p| p.cryptographic_audit_trail);
            if audit_enabled {
                let trace_id = T::meta(&args).and_then(|m| m.extract_trace_id());
                if let Err(e) = state.audit.record_entry(
                    "active-session",
                    trace_id.as_deref(),
                    T::NAME,
                    &serde_json::to_string(&args).unwrap_or_default(),
                    status,
                    files,
                    redacted,
                ) {
                    tracing::warn!(target: "mesh::audit", "Audit write failed for {}: {e}", T::NAME);
                }
            }

            result.map(|out| out.text)
        })
        .await
        .map_err(|e| (-32603, format!("Tool task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{AuditLogger, BackgroundRescanEngine, Config};
    use serde::Deserialize;

    /// `[engines.policy] cryptographic_audit_trail` must actually gate whether a
    /// tool call is written to the audit log, instead of the log always being
    /// written unconditionally.
    #[tokio::test]
    async fn cryptographic_audit_trail_flag_gates_audit_writes() {
        async fn run_with(enabled: bool) -> usize {
            let cfg_str = format!(
                "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.policy]\ncryptographic_audit_trail = {enabled}\n"
            );
            let config = Config::load_from_str(&cfg_str).expect("config");
            let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
            let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
            let state = Arc::new(AppState::new(config, vec![], audit.clone(), rescan));

            let _ =
                ToolRegistry::call_tool(SearchDocsTool::NAME, json!({"query": "anything"}), state)
                    .await;

            let dest = tempfile::NamedTempFile::new().expect("tmp file");
            audit.export_to_jsonl(dest.path()).expect("export")
        }

        assert_eq!(
            run_with(true).await,
            1,
            "cryptographic_audit_trail = true must record the call"
        );
        assert_eq!(
            run_with(false).await,
            0,
            "cryptographic_audit_trail = false must skip the audit write"
        );
    }

    /// Test-only stand-in for a future mutating tool. It carries a fixed
    /// `mutates() -> true` so the governance wiring in `invoke` has something
    /// real to check against, since none of MeshMCP's shipped tools mutate
    /// anything today.
    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
    struct MutatingToolArgs {
        target: String,
    }

    struct MutatingTestTool;

    impl McpTool for MutatingTestTool {
        const NAME: &'static str = "test_mutating_tool";
        const DESCRIPTION: &'static str = "Test-only tool that mutates its target.";
        type Args = MutatingToolArgs;

        fn meta(_args: &Self::Args) -> Option<&RequestMeta> {
            None
        }

        fn run(args: &Self::Args, _state: &AppState) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(format!("mutated {}", args.target)))
        }

        fn subject(args: &Self::Args) -> Option<&str> {
            Some(args.target.as_str())
        }

        fn mutates(_args: &Self::Args) -> bool {
            true
        }
    }

    fn governed_state() -> Arc<AppState> {
        let config = Config::load_from_str(
            r#"
[workspace]
name = "governance-test"
version = "0"
roots = ["."]

[engines.policy.stop_rules]
"proto-registry" = "STOP CASCADE CI"
"#,
        )
        .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        Arc::new(AppState::new(config, vec![], audit, rescan))
    }

    /// A tool call against a guarded subject is refused by RSAH through the
    /// real `ToolRegistry::invoke` path (not just a direct `evaluate_guard` unit test).
    #[tokio::test]
    async fn test_invoke_blocks_mutating_call_on_guarded_subject() {
        let state = governed_state();
        let args = json!({ "target": "services/proto-registry/auth.proto" });

        let result = ToolRegistry::invoke::<MutatingTestTool>(args, state).await;
        let (code, message) = result.expect_err("guarded mutation must be refused");
        assert_eq!(code, GOVERNANCE_BLOCKED_CODE);

        let rsah: mesh_core::RsahResponse =
            serde_json::from_str(&message).expect("RSAH payload is valid JSON");
        assert_eq!(rsah.status, "GOVERNANCE_BLOCKED");
        assert_eq!(rsah.policy, "CONTRACT_FIRST_CASCADE_CI");
        assert_eq!(rsah.agent_next_action, "STOP_AND_REPORT_TO_USER");
        assert!(!rsah.message_to_user.is_empty());
        // The old French copy must not resurface through this production path.
        assert!(rsah.message_to_user.is_ascii());
    }

    /// A read-only call against the same guarded subject must stay allowed:
    /// governance is reserved for mutations.
    #[tokio::test]
    async fn test_invoke_allows_read_only_call_on_guarded_subject() {
        let state = governed_state();
        let args = json!({ "target": "services/proto-registry/auth.proto" });

        struct ReadOnlyTestTool;
        impl McpTool for ReadOnlyTestTool {
            const NAME: &'static str = "test_readonly_tool";
            const DESCRIPTION: &'static str = "Test-only read-only tool.";
            type Args = MutatingToolArgs;

            fn meta(_args: &Self::Args) -> Option<&RequestMeta> {
                None
            }

            fn run(args: &Self::Args, _state: &AppState) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text(format!("read {}", args.target)))
            }

            fn subject(args: &Self::Args) -> Option<&str> {
                Some(args.target.as_str())
            }
        }

        let result = ToolRegistry::invoke::<ReadOnlyTestTool>(args, state).await;
        let text = result.expect("read-only call on a guarded subject stays allowed");
        assert!(text.contains("read services/proto-registry/auth.proto"));
    }

    #[test]
    fn test_list_tools_contains_all_tools() {
        let tools = ToolRegistry::list_tools();
        let arr = tools.as_array().expect("tools array");
        assert_eq!(arr.len(), 6);
        let names: Vec<_> = arr.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"smart_search"));
        assert!(names.contains(&"find_dependents"));
        assert!(names.contains(&"analyze_grpc"));
        assert!(names.contains(&"analyze_impact"));
        assert!(names.contains(&"search_docs"));
        assert!(names.contains(&"visualize_mesh"));
    }

    /// Regression test for Issue 3: assert generated JSON schemas match real tool structs
    #[test]
    fn test_tool_schemas_match_expectations() {
        use schemars::schema_for;

        let find_deps = schema_for!(super::find_dependents::FindDependentsArgs);
        let find_deps_props: Vec<String> = find_deps
            .schema
            .object
            .unwrap()
            .properties
            .keys()
            .cloned()
            .collect();
        assert!(find_deps_props.contains(&"target".to_string()));
        assert!(!find_deps_props.contains(&"scope".to_string()));

        let analyze_grpc = schema_for!(super::analyze_grpc::AnalyzeGrpcArgs);
        let grpc_props: Vec<String> = analyze_grpc
            .schema
            .object
            .unwrap()
            .properties
            .keys()
            .cloned()
            .collect();
        assert!(grpc_props.contains(&"target".to_string()));
        assert!(!grpc_props.contains(&"service_name".to_string()));

        let analyze_impact = schema_for!(super::analyze_impact::AnalyzeImpactArgs);
        let impact_props: Vec<String> = analyze_impact
            .schema
            .object
            .unwrap()
            .properties
            .keys()
            .cloned()
            .collect();
        assert!(impact_props.contains(&"target".to_string()));
        assert!(!impact_props.contains(&"changed_file".to_string()));

        let search_docs = schema_for!(super::search_docs::SearchDocsArgs);
        let docs_props: Vec<String> = search_docs
            .schema
            .object
            .unwrap()
            .properties
            .keys()
            .cloned()
            .collect();
        assert!(docs_props.contains(&"query".to_string()));
        assert!(docs_props.contains(&"max_sections".to_string()));
        assert!(!docs_props.contains(&"scope".to_string()));
    }

    #[test]
    fn test_mcp_tools_md_schema_drift_check() {
        let path = std::path::Path::new("docs/mcp-tools.md");
        if !path.exists() {
            return;
        }
        let doc_text = std::fs::read_to_string(path).expect("read mcp-tools.md");

        // Parse tool sections starting with `### Tool `
        let sections: Vec<&str> = doc_text.split("### Tool ").skip(1).collect();
        assert!(
            !sections.is_empty(),
            "docs/mcp-tools.md must contain tool sections"
        );

        for section in sections {
            // Find the `#### JSON Schema` heading
            let Some(schema_idx) = section.find("#### JSON Schema") else {
                continue;
            };
            let schema_part = &section[schema_idx..];

            // Extract the first ```json ... ``` code block under `#### JSON Schema`
            let Some(code_open) = schema_part.find("```json") else {
                continue;
            };
            let code_start = code_open + 7;
            let Some(code_close) = schema_part[code_start..].find("```") else {
                continue;
            };
            let json_str = schema_part[code_start..code_start + code_close].trim();

            let schema_val: serde_json::Value =
                serde_json::from_str(json_str).expect("parse JSON Schema from docs/mcp-tools.md");
            let props = schema_val
                .get("properties")
                .and_then(|p| p.as_object())
                .expect("JSON schema in docs/mcp-tools.md must have properties object");

            let prop_keys: Vec<&String> = props.keys().collect();

            // Verify that no obsolete field names exist in any tool schema block
            assert!(
                !prop_keys.contains(&&"service_name".to_string()),
                "Schema block must not contain obsolete field 'service_name'"
            );
            assert!(
                !prop_keys.contains(&&"method_name".to_string()),
                "Schema block must not contain obsolete field 'method_name'"
            );
            assert!(
                !prop_keys.contains(&&"changed_file".to_string()),
                "Schema block must not contain obsolete field 'changed_file'"
            );

            // Verify that required fields match real tool structs
            if section.contains("`find_dependents`")
                || section.contains("`analyze_grpc`")
                || section.contains("`analyze_impact`")
            {
                assert!(
                    prop_keys.contains(&&"target".to_string()),
                    "Schema block for target tools must contain 'target'"
                );
            }
            if section.contains("`smart_search`") {
                assert!(
                    prop_keys.contains(&&"query".to_string())
                        && prop_keys.contains(&&"scope".to_string()),
                    "Schema block for smart_search must contain 'query' and 'scope'"
                );
            }
            if section.contains("`search_docs`") {
                assert!(
                    prop_keys.contains(&&"query".to_string()),
                    "Schema block for search_docs must contain 'query'"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_truncation_hints_contain_arguments() {
        let config = Config::load_from_str(
            "[workspace]\nname = \"test\"\nversion = \"1.0.0\"\nroots = [\".\"]",
        )
        .expect("config");
        let allowed_roots = vec![std::path::PathBuf::from(".")];
        let audit = Arc::new(mesh_core::AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(mesh_core::BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

        let find_deps_args = super::find_dependents::FindDependentsArgs {
            target: mesh_core::CompactStr::new("UserAuthRequest"),
            _meta: None,
        };
        let hint_deps = FindDependentsTool::truncation_hint(&find_deps_args, &state).unwrap();
        assert!(
            hint_deps.contains("UserAuthRequest"),
            "FindDependents truncation hint must contain target argument"
        );

        let search_args = super::smart_search::SmartSearchArgs {
            query: mesh_core::CompactStr::new("signUp"),
            scope: mesh_core::CompactStr::new("ms-user"),
            include_body: false,
            fuzzy: None,
            _meta: None,
        };
        let hint_search = SmartSearchTool::truncation_hint(&search_args, &state).unwrap();
        assert!(
            hint_search.contains("signUp"),
            "SmartSearch truncation hint must contain query argument"
        );
        assert!(
            hint_search.contains("ms-user"),
            "SmartSearch truncation hint must contain scope argument"
        );

        let impact_args = super::analyze_impact::AnalyzeImpactArgs {
            target: mesh_core::CompactStr::new("EVENT_CREATED"),
            _meta: None,
        };
        let hint_impact = AnalyzeImpactTool::truncation_hint(&impact_args, &state).unwrap();
        assert!(
            hint_impact.contains("EVENT_CREATED"),
            "AnalyzeImpact truncation hint must contain target argument"
        );
    }
}
