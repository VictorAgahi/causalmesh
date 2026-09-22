pub mod analyze_grpc;
pub mod analyze_impact;
pub mod find_dependents;
pub mod search_docs;
pub mod smart_search;
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

        // Active governance (RSAH): only mutating calls are subject to a stop
        // rule. Read-only tools (search, analyze_impact, find_dependents, ...)
        // never call `evaluate_guard`, so inspecting a guarded scope stays allowed.
        if T::mutates(&args) {
            if let Some(subject) = T::subject(&args) {
                if let Some(rsah) = state.governance.evaluate_guard(subject) {
                    let payload = serde_json::to_string(&rsah).unwrap_or_else(|_| {
                        "RSAH governance refusal (payload serialization failed)".to_string()
                    });
                    return Err((GOVERNANCE_BLOCKED_CODE, payload));
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
            }

            let (status, files, redacted) = match &result {
                Ok(out) => ("SUCCESS", out.files_accessed.clone(), out.secrets_redacted),
                Err(_) => ("ERROR", Vec::new(), 0),
            };

            // Audit is best-effort and off the executor; the SQLite write never
            // gates the response but always happens on the same blocking thread.
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

    /// Definition of done for ROADMAP item 4: a tool call against a guarded
    /// subject is refused by RSAH through the real `ToolRegistry::invoke` path
    /// (not just a direct `evaluate_guard` unit test).
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
}
