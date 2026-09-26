//! Test-only tool that deliberately blocks its executor thread for a bounded
//! duration. It exists solely so that daemon integration tests can exercise
//! the real `ToolRegistry::invoke` -> `spawn_blocking` path with a slow call,
//! without depending on the timing characteristics of a real tool such as
//! `smart_search`. Compiled only when the `test-util` feature is enabled
//! (mesh-daemon's dev-dependencies only — never in a release binary).

use crate::protocol::RequestMeta;
use crate::tools::{McpTool, ToolError, ToolOutput};
use mesh_core::AppState;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestSlowOpArgs {
    /// How long to block the executing thread for, in milliseconds.
    #[serde(default = "default_sleep_ms")]
    pub sleep_ms: u64,

    #[serde(default)]
    // Accepted for W3C trace propagation, hidden from `tools/list`: the model
    // cannot use it, and it cost every session ~200 schema tokens.
    #[schemars(skip)]
    pub _meta: Option<RequestMeta>,
}

fn default_sleep_ms() -> u64 {
    500
}

pub struct TestSlowOpTool;

impl McpTool for TestSlowOpTool {
    const NAME: &'static str = "test_slow_op";
    const DESCRIPTION: &'static str =
        "TEST-ONLY: blocks the calling thread for `sleep_ms` milliseconds. Never call in production; only compiled under the test-util feature.";
    type Args = TestSlowOpArgs;

    fn meta(args: &Self::Args) -> Option<&RequestMeta> {
        args._meta.as_ref()
    }

    fn run(args: &Self::Args, _state: &AppState) -> Result<ToolOutput, ToolError> {
        std::thread::sleep(Duration::from_millis(args.sleep_ms));
        Ok(ToolOutput::text("slept".to_string()))
    }
}
