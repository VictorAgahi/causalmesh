#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! 7.1.10: `analyze_impact` never words a zero as proof of absence, and tells a depth stop from
//! the end of a chain. The fixture `examples/unresolved-flow` is a Redis Streams chain whose
//! middle hop publishes to `cfg.stream` / `cfg.type` read from a runtime registry, so no literal
//! `xadd` exists for that hop. It also holds a 7-file chain, a fan-out of 7 consumers, a cycle and
//! nodes that only a test file declares.

use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config};
use mesh_server::tools::ToolRegistry;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn fixture_dir() -> std::path::PathBuf {
    dunce::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/unresolved-flow"),
    )
    .expect("canonicalize fixture")
}

fn state_for(dir: &Path) -> Arc<AppState> {
    let config = Config::load_from_file(&dir.join("mesh-mcp.toml")).expect("fixture config");
    let roots = expand_roots(
        &config.workspace.roots,
        dir,
        &config.workspace.workspace_root,
    )
    .expect("expand roots");
    let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
    let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
    let state = Arc::new(AppState::new(config, roots, audit, rescan));
    let snapshot = mesh_server::WorkspaceIndexer::build_snapshot(
        &state.config,
        &state.allowed_roots,
        None,
        None,
        None,
    );
    state.install_snapshot(snapshot);
    state
}

fn fixture() -> Arc<AppState> {
    state_for(&fixture_dir())
}

async fn impact(state: &Arc<AppState>, args: Value) -> String {
    let res = ToolRegistry::call_tool("analyze_impact", args, state.clone())
        .await
        .expect("tool call");
    assert_eq!(res["isError"], json!(false), "{res}");
    res["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string()
}

const NO_PRODUCER: &str = "**No producer statically resolved**";
const NO_CONSUMER: &str = "**No consumer statically resolved**";
const DEPTH_LIMIT: &str = "**Depth limit reached**";
const CHAIN_ENDS: &str = "**Chain ends**";

/// The header notes only, without the table.
fn notes(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with('|'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

#[tokio::test]
async fn consumer_without_resolved_producer_is_not_statically_resolved_not_absent() {
    let state = fixture();
    let text = impact(&state, json!({"target": "user:deletion:success"})).await;
    assert!(
        text.contains("**No producer statically resolved** for this event outside tests: a consumer is indexed, but no literal producer call site was found across 1 workspace roots. This is not proof that nothing emits it: the producer may publish under a name resolved at runtime (registry, config, template string) or live in a root that is not indexed."),
        "{text}"
    );
    assert!(
        text.contains("*A `[[engines.contracts.patterns]]` of kind `topic_producer` can close this gap for a project-specific registry (see SETUP.md).*"),
        "{text}"
    );
    let lower = text.to_lowercase();
    assert!(!lower.contains("authoritative"), "{text}");
    assert!(
        !lower.contains("nothing in static application code"),
        "{text}"
    );
    assert!(text.contains("listenDeletionSuccess"), "{text}");
}

#[tokio::test]
async fn default_depth_with_a_non_literal_onward_publish_shows_the_neutral_depth_note() {
    let state = fixture();
    // `listenDeletionRequests` re-emits through `cfg.stream`: no resolved publish at all.
    let text = impact(&state, json!({"target": "user:deletion:requested"})).await;
    assert!(
        text.contains("**Depth limit reached** (depth 1): 1 consumer(s) were not expanded: `listenDeletionRequests` (`src/gather.processor.ts:6`). Repeat with `depth: 2` to follow them."),
        "{text}"
    );
    assert!(!text.contains(CHAIN_ENDS), "{text}");
    assert!(
        !text.contains(NO_PRODUCER) && !text.contains(NO_CONSUMER),
        "{text}"
    );
}

#[tokio::test]
async fn depth_two_ending_at_a_runtime_hop_says_chain_ends_without_a_pattern_pointer() {
    let state = fixture();
    for depth in [2, 3] {
        let text = impact(
            &state,
            json!({"target": "user:deletion:requested", "depth": depth}),
        )
        .await;
        assert!(
            text.contains("**Chain ends** at 1 consumer(s) with no statically resolved onward publish: `listenDeletionRequests` (`src/gather.processor.ts:6`). A terminal consumer is normal; if one of them publishes under a name resolved at runtime, the chain continues there."),
            "{text}"
        );
        assert!(
            !text.contains("SETUP.md"),
            "no pattern pointer on a chain end: {text}"
        );
        assert!(!text.contains("before concluding"), "{text}");
        assert!(!text.contains(DEPTH_LIMIT), "{text}");
        assert!(
            !text.contains(NO_PRODUCER) && !text.contains(NO_CONSUMER),
            "{text}"
        );
    }
}

#[tokio::test]
async fn depth_stop_is_distinguishable_from_a_real_end() {
    let state = fixture();
    // Depth 1 and 2: the walk stopped on `depth`, whatever lies past it.
    let d1 = impact(&state, json!({"target": "user:deletion:success"})).await;
    assert!(
        d1.contains("**Depth limit reached** (depth 1): 1 consumer(s) were not expanded: `listenDeletionSuccess` (`src/purge.consumer.ts:5`). Repeat with `depth: 2` to follow them."),
        "{d1}"
    );
    assert!(!d1.contains(CHAIN_ENDS), "{d1}");

    let d2 = impact(
        &state,
        json!({"target": "user:deletion:success", "depth": 2}),
    )
    .await;
    assert!(
        d2.contains("**Depth limit reached** (depth 2): 1 consumer(s) were not expanded: `listenCachePurged` (`src/audit.consumer.ts:5`). Repeat with `depth: 3` to follow them."),
        "{d2}"
    );
    assert!(!d2.contains(CHAIN_ENDS), "{d2}");

    // Depth 3 expands the last consumer and finds the real end.
    let d3 = impact(
        &state,
        json!({"target": "user:deletion:success", "depth": 3}),
    )
    .await;
    assert!(!d3.contains(DEPTH_LIMIT), "{d3}");
    assert!(
        d3.contains("**Chain ends** at 1 consumer(s) with no statically resolved onward publish: `listenCachePurged` (`src/audit.consumer.ts:5`)."),
        "{d3}"
    );
}

#[tokio::test]
async fn maximum_depth_says_the_downstream_is_not_shown() {
    let state = fixture();
    let text = impact(&state, json!({"target": "chain:0", "depth": 5})).await;
    assert!(
        text.contains("**Depth limit reached** (maximum depth 5): 1 consumer(s) were not expanded: `listenChain4` (`src/chain4.ts:5`). Their downstream is not shown."),
        "{text}"
    );
    assert!(!text.contains("depth: 6"), "{text}");
    // A larger request is clamped to the same maximum.
    let clamped = impact(&state, json!({"target": "chain:0", "depth": 200})).await;
    assert_eq!(text, clamped);
}

#[tokio::test]
async fn more_than_five_nodes_are_truncated_with_a_count() {
    let state = fixture();
    let text = impact(&state, json!({"target": "fan:out"})).await;
    assert!(
        text.contains("**Depth limit reached** (depth 1): 7 consumer(s) were not expanded: `fanConsumer0` (`src/fan.ts:9`), `fanConsumer1` (`src/fan.ts:13`), `fanConsumer2` (`src/fan.ts:17`), `fanConsumer3` (`src/fan.ts:21`), `fanConsumer4` (`src/fan.ts:25`), +2 more. Repeat with `depth: 2` to follow them."),
        "{text}"
    );
    assert!(!notes(&text).contains("fanConsumer5"), "{text}");
}

#[tokio::test]
async fn a_cycle_terminates_and_is_stable() {
    let state = fixture();
    let a = impact(&state, json!({"target": "cycle:a", "depth": 4})).await;
    let b = impact(&state, json!({"target": "cycle:a", "depth": 4})).await;
    assert_eq!(a, b);
    // Both handlers re-emit onto the cycle, so neither is a chain end or a depth stop.
    assert!(!notes(&a).contains(CHAIN_ENDS), "{a}");
    assert!(!notes(&a).contains(DEPTH_LIMIT), "{a}");
    assert!(
        a.contains("listenCycleA") && a.contains("listenCycleB"),
        "{a}"
    );
    // The notes say a consumer is listed once.
    assert_eq!(a.matches("| consumer | `listenCycleA`").count(), 1, "{a}");
}

#[tokio::test]
async fn event_type_only_in_a_registry_matches_nothing_and_says_so() {
    let state = fixture();
    let text = impact(&state, json!({"target": "user.deletion.success"})).await;
    assert!(text.contains("(0 matches)"), "{text}");
    assert!(
        text.contains("A name that only a runtime registry, config or template string produces is not indexed under that name: try the literal stream or topic of a neighbouring hop."),
        "{text}"
    );
}

#[tokio::test]
async fn orphans_get_neutral_wording_in_both_directions() {
    let state = fixture();
    let consumer_only = impact(&state, json!({"target": "legacy:orders"})).await;
    assert!(consumer_only.contains(NO_PRODUCER), "{consumer_only}");
    assert!(!consumer_only.contains(NO_CONSUMER), "{consumer_only}");
    assert!(
        consumer_only.contains("may publish under a name resolved at runtime"),
        "{consumer_only}"
    );

    let producer_only = impact(&state, json!({"target": "metrics:emitted"})).await;
    assert!(
        producer_only.contains("**No consumer statically resolved** for this event outside tests: a producer is indexed, but no literal consumer registration was found across 1 workspace roots. This is not proof that nobody consumes it: a consumer may be registered dynamically, through a pattern that is not in the config, or live in a root that is not indexed."),
        "{producer_only}"
    );
    assert!(!producer_only.contains(NO_PRODUCER), "{producer_only}");
    assert!(
        producer_only.contains("kind `topic_consumer`"),
        "{producer_only}"
    );
    assert!(
        !producer_only.to_lowercase().contains("authoritative"),
        "{producer_only}"
    );
}

#[tokio::test]
async fn a_consumer_that_only_a_test_file_declares_is_not_counted_as_resolved() {
    let state = fixture();
    let hidden = impact(&state, json!({"target": "ghost:events"})).await;
    assert!(hidden.contains(NO_CONSUMER), "{hidden}");
    assert!(hidden.contains("a producer is indexed"), "{hidden}");
    assert!(!hidden.contains(NO_PRODUCER), "{hidden}");
    assert!(hidden.contains("row(s) in test files left out"), "{hidden}");
    assert!(!hidden.contains("ghost.spec.ts"), "{hidden}");

    let shown = impact(
        &state,
        json!({"target": "ghost:events", "include_tests": true}),
    )
    .await;
    assert!(!shown.contains(NO_CONSUMER), "{shown}");
    assert!(shown.contains("listenGhostEventsInTest"), "{shown}");
}

#[tokio::test]
async fn a_producer_that_only_a_test_file_declares_is_not_counted_as_resolved() {
    let state = fixture();
    let hidden = impact(&state, json!({"target": "ghost:inbox"})).await;
    assert!(hidden.contains(NO_PRODUCER), "{hidden}");
    assert!(hidden.contains("a consumer is indexed"), "{hidden}");
    assert!(!hidden.contains("ghost.spec.ts"), "{hidden}");
    let shown = impact(
        &state,
        json!({"target": "ghost:inbox", "include_tests": true}),
    )
    .await;
    assert!(!shown.contains(NO_PRODUCER), "{shown}");
    assert!(shown.contains("emitGhostInboxInTest"), "{shown}");
}

#[tokio::test]
async fn an_event_indexed_only_in_test_files_says_so() {
    let state = fixture();
    let text = impact(&state, json!({"target": "ghost:only"})).await;
    assert!(
        text.contains(NO_PRODUCER) && text.contains(NO_CONSUMER),
        "{text}"
    );
    assert!(
        text.contains("the event is indexed only in test files (`include_tests: true` lists them)"),
        "{text}"
    );
    assert!(!text.contains("a consumer is indexed"), "{text}");
    assert!(!text.contains("the topic is indexed"), "{text}");
}

#[tokio::test]
async fn a_workspace_root_under_a_directory_named_tests_is_not_all_test_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("tests").join("e2e").join("unresolved-flow");
    copy_dir(&fixture_dir(), &root);
    let root = dunce::canonicalize(&root).expect("canonicalize");
    let state = state_for(&root);
    let text = impact(
        &state,
        json!({"target": "user:deletion:requested", "depth": 2}),
    )
    .await;
    assert!(text.contains("requestUserDeletion"), "{text}");
    assert!(text.contains("listenDeletionRequests"), "{text}");
    assert!(text.contains(CHAIN_ENDS), "{text}");
    assert!(!text.contains("row(s) in test files left out"), "{text}");
    // Same notes as the in-repo fixture, and the spec file is still a test.
    let ghost = impact(&state, json!({"target": "ghost:events"})).await;
    assert!(ghost.contains(NO_CONSUMER), "{ghost}");
    assert!(ghost.contains("row(s) in test files left out"), "{ghost}");
}

#[tokio::test]
async fn resolved_pair_gets_no_unresolved_note_and_no_pattern_pointer() {
    let state = fixture();
    for depth in [1, 2] {
        let text = impact(&state, json!({"target": "cache:purged", "depth": depth})).await;
        let header = notes(&text);
        for marker in [NO_PRODUCER, NO_CONSUMER, "SETUP.md", "not proof"] {
            assert!(!header.contains(marker), "unexpected {marker}: {text}");
        }
        assert!(
            text.contains("purgeCaches") && text.contains("listenCachePurged"),
            "{text}"
        );
    }
    // Depth 1 reached the consumer without expanding it; depth 2 expanded it and it is terminal.
    let d1 = impact(&state, json!({"target": "cache:purged"})).await;
    assert!(d1.contains("**Depth limit reached** (depth 1): 1 consumer(s) were not expanded: `listenCachePurged`"), "{d1}");
    let d2 = impact(&state, json!({"target": "cache:purged", "depth": 2})).await;
    assert!(!d2.contains(DEPTH_LIMIT), "{d2}");
    assert!(d2.contains("**Chain ends** at 1 consumer(s)"), "{d2}");
}

#[tokio::test]
async fn notes_are_deterministic_and_bounded() {
    let state = fixture();
    let args = json!({"target": "fan:out", "depth": 3});
    let a = impact(&state, args.clone()).await;
    let b = impact(&state, args).await;
    assert_eq!(a, b);
    let header = notes(&a);
    assert!(header.len() < 2_000, "{} bytes: {header}", header.len());
}
