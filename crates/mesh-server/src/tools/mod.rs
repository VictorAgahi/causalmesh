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
/// -32602 (invalid params). Tool-level codes like this one classify a failure
/// internally; `call_tool` surfaces every one of them to the client as a
/// `CallToolResult` with `isError: true`, never as a JSON-RPC error.
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

    /// Whether this call returns a machine-readable document (JSON, HTML) rather
    /// than Markdown prose. Nothing is appended to such a document — no skill
    /// footer — since any trailing text would make it unparseable.
    fn returns_document(_args: &Self::Args) -> bool {
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

    /// Dispatches an incoming MCP `tools/call` request.
    ///
    /// Per the MCP specification (2024-11-05), a failure *inside* a tool —
    /// invalid arguments, a scope outside the sandbox jail, a missing target, an
    /// RSAH governance refusal — is not a protocol error: it is returned as a
    /// successful JSON-RPC response carrying a `CallToolResult` with
    /// `isError: true` and the message as text content, so the client hands it
    /// to the model as the tool's answer and the agent can self-correct. Only
    /// protocol-level faults come back as `Err` for the caller to send as a
    /// JSON-RPC error: an unknown tool name (`-32602`, as the MCP spec
    /// prescribes) and an internal failure such as a panicked tool task
    /// (`-32603`).
    pub async fn call_tool(
        name: &str,
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<Value, ToolError> {
        let outcome = match name {
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
            unknown => return Err((-32602, format!("Unknown tool: {unknown}"))),
        };

        Ok(match outcome {
            Ok(text) => Self::tool_result(text, false),
            Err((_, message)) => Self::tool_result(message, true),
        })
    }

    /// An MCP `CallToolResult` with one text content block.
    pub fn tool_result(text: String, is_error: bool) -> Value {
        json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": is_error
        })
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
    /// Runs one tool. The outer `Result` is a protocol-level fault (the tool task
    /// itself failed); the inner one is the tool's own outcome, which
    /// `call_tool` turns into a `CallToolResult` (`isError` on `Err`).
    async fn invoke<T: McpTool>(
        arguments: Value,
        state: Arc<AppState>,
    ) -> Result<Result<String, ToolError>, ToolError> {
        let args: T::Args = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(e) => {
                return Ok(Err((
                    -32602,
                    format!("Invalid arguments for {}: {e}", T::NAME),
                )))
            }
        };

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
                        return Ok(Err((GOVERNANCE_BLOCKED_CODE, payload)));
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
                if !T::returns_document(&args) {
                    if let Some(skill) =
                        state.governance.recommend_skill(T::NAME, T::subject(&args))
                    {
                        out.text.push_str(&Self::render_skill_hint(skill));
                    }
                }

                // Plan 4 step 4.2: while a Git operation holds reloads back, say which
                // (complete) index generation answered. Prepended, so the 48 KB cap below
                // can never cut it off.
                if let Some(note) = mesh_core::FileWatcherService::git_operation_note(&state) {
                    out.text.insert_str(0, &note);
                }

                // Centralized 48 KB Payload Budget Capping
                const MAX_TOOL_OUTPUT_BYTES: usize = 48 * 1024;
                if out.text.len() > MAX_TOOL_OUTPUT_BYTES {
                    let hint = T::truncation_hint(&args, &state).unwrap_or_else(|| {
                        "Refine scope or pass specific search targets to narrow output.".to_string()
                    });
                    // Budget the note first: the cut text, its closing fence and
                    // the note together stay within the cap.
                    let note = truncation_note(&hint);
                    truncate_markdown(&mut out.text, MAX_TOOL_OUTPUT_BYTES - note.len());
                    out.text.push_str(&note);
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
        .map_err(|e| (-32603, format!("Tool task failed: {e}")))
    }
}

/// Longest truncation hint kept in the truncation note. Hints echo caller
/// arguments (`smart_search`'s query and scope), which are unbounded: without a
/// cap, the note alone could push the payload past the 48 KB it enforces.
const MAX_TRUNCATION_HINT_BYTES: usize = 512;

/// The note appended to a truncated payload. One line of prose: newlines in the
/// hint are flattened so an echoed argument cannot open a code fence or a new
/// Markdown block after the cut.
fn truncation_note(hint: &str) -> String {
    let mut hint = hint.replace(['\n', '\r'], " ");
    if hint.len() > MAX_TRUNCATION_HINT_BYTES {
        let mut cut = MAX_TRUNCATION_HINT_BYTES;
        while !hint.is_char_boundary(cut) {
            cut -= 1;
        }
        hint.truncate(cut);
        hint.push('…');
    }
    format!(
        "\n\n> [!NOTE]\n> Output payload truncated to fit within maximum MCP output payload cap (48 KB). {hint}\n"
    )
}

/// The fence that is still open at the end of `text`, if any (e.g. ```` ``` ````
/// or `~~~~`), following CommonMark: an opening fence is 3+ backticks or tildes
/// indented by at most 3 spaces (a backtick fence's info string cannot contain a
/// backtick); inside it, only a line of at least as many of the *same* character
/// and nothing else closes it — a ```` ```rust ```` line inside a block is content.
fn open_fence(text: &str) -> Option<String> {
    let mut open: Option<(char, usize)> = None;
    for line in text.lines() {
        let rest = line.trim_start_matches(' ');
        if line.len() - rest.len() > 3 {
            continue;
        }
        let Some(c) = rest.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let run = rest.len() - rest.trim_start_matches(c).len();
        if run < 3 {
            continue;
        }
        let after = &rest[run..];
        match open {
            None if !(c == '`' && after.contains('`')) => open = Some((c, run)),
            Some((oc, on)) if c == oc && run >= on && after.trim().is_empty() => open = None,
            _ => {}
        }
    }
    open.map(|(c, n)| std::iter::repeat_n(c, n).collect())
}

/// Byte offset `text` is cut at for a `budget`: a char boundary, backed off to
/// the last complete line when there is one in the kept half (a single huge
/// line is cut mid-line rather than dropped).
fn line_cut(text: &str, budget: usize) -> usize {
    let mut cut = budget.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    match text[..cut].rfind('\n') {
        Some(nl) if nl >= cut / 2 => nl,
        _ => cut,
    }
}

/// Cuts `text` to at most `budget` bytes — the closing fence included — on a
/// line boundary, and closes any Markdown code fence left open, so a truncated
/// payload never ends mid-token or inside an unterminated block (which makes
/// everything after it, including the truncation note, parse as code downstream).
fn truncate_markdown(text: &mut String, budget: usize) {
    if text.len() <= budget {
        return;
    }
    // Room kept for the closing fence. It only grows (a larger reserve is only
    // taken when the fence found needs more than the current one), so this ends.
    let mut reserve = 0usize;
    loop {
        let cut = line_cut(text, budget.saturating_sub(reserve));
        let fence = open_fence(&text[..cut]);
        let need = fence.as_ref().map_or(0, |f| f.len() + 1);
        if cut + need <= budget || need <= reserve || reserve >= budget {
            text.truncate(cut);
            if let Some(f) = fence {
                text.push('\n');
                text.push_str(&f);
            }
            return;
        }
        reserve = need;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_markdown_closes_open_fence_on_a_line_boundary() {
        let mut text = String::from("## Title\n```rust\nfn a() {}\nfn b() {}\nfn c() {}\n```\n");
        truncate_markdown(&mut text, 30);
        assert!(text.ends_with("\n```"), "{text:?}");
        assert_eq!(text.matches("```").count(), 2, "{text:?}");
        assert!(
            !text.contains("fn b() {"),
            "cut on a line boundary: {text:?}"
        );

        // Already-balanced output is only cut, never given a stray fence.
        let mut balanced = String::from("```\na\n```\nplain text that is long enough\n");
        truncate_markdown(&mut balanced, 20);
        assert_eq!(balanced.matches("```").count() % 2, 0, "{balanced:?}");

        // Multi-byte text never splits a char.
        let mut utf8 = "é".repeat(100);
        truncate_markdown(&mut utf8, 51);
        assert!(utf8.len() <= 51);
    }

    #[test]
    fn open_fence_follows_commonmark() {
        // A ```rust line inside an open block is content, not a close.
        assert_eq!(
            open_fence("```\ncode\n```rust\nmore").as_deref(),
            Some("```")
        );
        // Tilde fences, closed only by tildes.
        assert_eq!(open_fence("~~~\n```\n```\n").as_deref(), Some("~~~"));
        assert_eq!(open_fence("~~~\nx\n~~~\n"), None);
        // A 4-backtick fence is closed by 4+, not by an inner ```.
        assert_eq!(
            open_fence("````md\n```rust\nfn a() {}\n```\n").as_deref(),
            Some("````")
        );
        // Indented by 4 spaces: an indented code line, not a fence.
        assert_eq!(open_fence("    ```\ntext\n"), None);
        // Backticks in a backtick fence's info string: inline code, not a fence.
        assert_eq!(open_fence("```a``` inline\ntext\n"), None);
        assert_eq!(open_fence("use `x` and ``` y\n"), None);
    }

    #[test]
    fn truncation_stays_within_budget_with_fence_and_note() {
        // A long fence opener must still fit: cut + "\n" + fence <= budget.
        let fence = "`".repeat(40);
        let mut text = format!("{fence}\n{}", "line of code\n".repeat(50));
        truncate_markdown(&mut text, 100);
        assert!(text.len() <= 100, "{} > 100", text.len());
        assert!(text.ends_with(&fence));

        // An unbounded hint (echoed query) is capped and flattened.
        let note = truncation_note(&format!("```\n{}", "q".repeat(100_000)));
        assert!(note.len() < 1024, "{}", note.len());
        assert_eq!(note.matches('\n').count(), 4, "{note:?}");
    }
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

        let result = ToolRegistry::invoke::<MutatingTestTool>(args, state)
            .await
            .expect("a governance refusal is a tool outcome, not a protocol fault");
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

        let result = ToolRegistry::invoke::<ReadOnlyTestTool>(args, state)
            .await
            .expect("no protocol fault");
        let text = result.expect("read-only call on a guarded subject stays allowed");
        assert!(text.contains("read services/proto-registry/auth.proto"));
    }

    /// The 48 KB cap holds for the whole payload — cut text, closing fence and
    /// note — even when the tool's hint echoes a huge argument.
    #[tokio::test]
    async fn test_invoke_caps_output_including_note_and_fence() {
        struct HugeTool;
        impl McpTool for HugeTool {
            const NAME: &'static str = "test_huge_tool";
            const DESCRIPTION: &'static str = "Test-only tool with an oversized answer.";
            type Args = MutatingToolArgs;
            fn meta(_args: &Self::Args) -> Option<&RequestMeta> {
                None
            }
            fn run(_args: &Self::Args, _state: &AppState) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text(format!(
                    "```rust\n{}",
                    "fn f() {}\n".repeat(20_000)
                )))
            }
            fn truncation_hint(args: &Self::Args, _state: &AppState) -> Option<String> {
                Some(format!("Query '{}' is too broad.", args.target))
            }
        }
        let args = json!({ "target": "x".repeat(200_000) });
        let text = ToolRegistry::invoke::<HugeTool>(args, governed_state())
            .await
            .expect("no protocol fault")
            .expect("tool ok");
        assert!(text.len() <= 48 * 1024, "{} bytes", text.len());
        assert_eq!(open_fence(&text), None, "fence closed before the note");
        assert!(text.contains("> [!NOTE]"));
    }

    /// Internal W3C trace context is accepted on input but never advertised to
    /// the model in `tools/list`.
    #[test]
    fn test_list_tools_hides_request_meta() {
        let listed = ToolRegistry::list_tools().to_string();
        assert!(!listed.contains("_meta"), "{listed}");
        assert!(!listed.contains("traceparent"), "{listed}");
        assert!(!listed.contains("RequestMeta"), "{listed}");
    }

    /// Hiding `_meta` from the schema must not make `deny_unknown_fields`
    /// reject it: every tool still accepts a W3C trace context, and every
    /// advertised schema still forbids unknown fields.
    #[tokio::test]
    async fn test_every_tool_accepts_hidden_meta() {
        let tools = ToolRegistry::list_tools();
        for tool in tools.as_array().expect("tools array") {
            let name = tool["name"].as_str().expect("name");
            assert_eq!(
                tool["inputSchema"]["additionalProperties"], false,
                "{name} must still deny unknown fields"
            );
            let mut args = match name {
                "smart_search" | "search_docs" => json!({ "query": "x" }),
                "visualize_mesh" => json!({}),
                _ => json!({ "target": "x" }),
            };
            args["_meta"] = json!({
                "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            });
            let val = ToolRegistry::call_tool(name, args, governed_state())
                .await
                .expect("no protocol fault");
            let text = val["content"][0]["text"].as_str().unwrap_or_default();
            // Other arguments may be invalid for a given tool; `_meta` must not be.
            assert!(
                !text.contains("unknown field"),
                "{name} rejected _meta: {text}"
            );
        }
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
        // Resolved from the crate, not the cwd: `cargo test` runs in the crate
        // directory, where a bare "docs/mcp-tools.md" never exists and this
        // check used to pass vacuously.
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/mcp-tools.md"
        ));
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

            // The documented properties are exactly the advertised ones.
            let name = section
                .lines()
                .next()
                .and_then(|l| l.split('`').nth(1))
                .expect("section header names the tool");
            let tools = ToolRegistry::list_tools();
            let real = tools
                .as_array()
                .and_then(|t| t.iter().find(|t| t["name"] == name))
                .and_then(|t| t["inputSchema"]["properties"].as_object());
            assert!(
                real.is_some(),
                "docs/mcp-tools.md documents unknown tool {name}"
            );
            let real = real.expect("checked above");
            let mut documented: Vec<&String> = props.keys().collect();
            let mut advertised: Vec<&String> = real.keys().collect();
            documented.sort();
            advertised.sort();
            assert_eq!(
                documented, advertised,
                "docs/mcp-tools.md drifted for {name}"
            );
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
            granularity: None,
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
            limit: None,
            offset: None,
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
            depth: None,
            _meta: None,
        };
        let hint_impact = AnalyzeImpactTool::truncation_hint(&impact_args, &state).unwrap();
        assert!(
            hint_impact.contains("EVENT_CREATED"),
            "AnalyzeImpact truncation hint must contain target argument"
        );
    }
}
