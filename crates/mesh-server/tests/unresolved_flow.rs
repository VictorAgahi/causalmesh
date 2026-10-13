#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! 7.1.10: `analyze_impact` never words a zero as proof of absence, and tells a depth stop from
//! the end of a chain. The fixture `examples/unresolved-flow` is a Redis Streams chain whose
//! middle hop publishes to `cfg.stream` / `cfg.type` read from a runtime registry, so no literal
//! `xadd` exists for that hop.

use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config};
use mesh_server::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

fn fixture() -> Arc<AppState> {
    let dir = dunce::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/unresolved-flow"),
    )
    .expect("canonicalize fixture");
    let config = Config::load_from_file(&dir.join("mesh-mcp.toml")).expect("fixture config");
    let roots = expand_roots(
        &config.workspace.roots,
        &dir,
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

const NO_PRODUCER: &str = "No producer statically resolved";
const NO_CONSUMER: &str = "No consumer statically resolved";
const DEPTH_LIMIT: &str = "Depth limit reached";
const CHAIN_END: &str = "Chain end within depth";

#[tokio::test]
async fn consumer_without_resolved_producer_is_not_statically_resolved_not_absent() {
    let state = fixture();
    let text = impact(&state, json!({"target": "user:deletion:success"})).await;
    assert!(text.contains(NO_PRODUCER), "{text}");
    assert!(text.contains("a consumer is indexed"), "{text}");
    assert!(text.contains("registry, config, template string"), "{text}");
    assert!(
        text.contains("not indexed") || text.contains("not proof"),
        "{text}"
    );
    assert!(
        text.contains("`topic_producer`") && text.contains("SETUP.md"),
        "{text}"
    );
    let lower = text.to_lowercase();
    assert!(!lower.contains("authoritative"), "{text}");
    assert!(
        !lower.contains("nothing in static application code"),
        "{text}"
    );
    // The consumer is still listed.
    assert!(text.contains("listenDeletionSuccess"), "{text}");
}

#[tokio::test]
async fn depth_two_ending_at_a_runtime_hop_says_chain_end_not_silence() {
    let state = fixture();
    for depth in [2, 3] {
        let text = impact(
            &state,
            json!({"target": "user:deletion:requested", "depth": depth}),
        )
        .await;
        assert!(text.contains(&format!("{CHAIN_END} {depth}")), "{text}");
        assert!(text.contains("listenDeletionRequests"), "{text}");
        assert!(text.contains("src/gather.processor.ts:6"), "{text}");
        assert!(text.contains("resolved at runtime"), "{text}");
        // The entry itself has a resolved producer and consumer.
        assert!(!text.contains(NO_PRODUCER), "{text}");
        assert!(!text.contains(NO_CONSUMER), "{text}");
        assert!(!text.contains(DEPTH_LIMIT), "{text}");
    }
}

#[tokio::test]
async fn depth_stop_is_distinguishable_from_a_real_end() {
    let state = fixture();
    // Depth 1: the consumer of `user:deletion:success` has an onward publish
    // (`cache:purged`) that the walk did not follow.
    let d1 = impact(&state, json!({"target": "user:deletion:success"})).await;
    assert!(d1.contains("**Depth limit reached** (depth 1)"), "{d1}");
    assert!(d1.contains("1 consumer(s) were not expanded"), "{d1}");
    assert!(d1.contains("listenDeletionSuccess"), "{d1}");
    assert!(d1.contains("`depth: 2`"), "{d1}");
    assert!(!d1.contains(CHAIN_END), "{d1}");

    // Depth 2 follows it; the last consumer was reached and has no onward publish, and
    // that is not a depth stop either.
    let d2 = impact(
        &state,
        json!({"target": "user:deletion:success", "depth": 2}),
    )
    .await;
    assert!(!d2.contains(DEPTH_LIMIT), "{d2}");
    assert!(!d2.contains(CHAIN_END), "{d2}");
    assert!(d2.contains("listenCachePurged"), "{d2}");

    // Depth 3 expands that last consumer and finds the end.
    let d3 = impact(
        &state,
        json!({"target": "user:deletion:success", "depth": 3}),
    )
    .await;
    assert!(!d3.contains(DEPTH_LIMIT), "{d3}");
    assert!(d3.contains(&format!("{CHAIN_END} 3")), "{d3}");
    assert!(d3.contains("listenCachePurged"), "{d3}");
}

#[tokio::test]
async fn event_type_only_in_a_registry_matches_nothing_and_says_so() {
    let state = fixture();
    let text = impact(&state, json!({"target": "user.deletion.success"})).await;
    assert!(text.contains("(0 matches)"), "{text}");
    assert!(
        text.contains("runtime registry, config or template string produces is not indexed"),
        "{text}"
    );
}

#[tokio::test]
async fn orphans_get_neutral_wording_in_both_directions() {
    let state = fixture();
    let consumer_only = impact(&state, json!({"target": "legacy:orders"})).await;
    assert!(consumer_only.contains(NO_PRODUCER), "{consumer_only}");
    assert!(!consumer_only.contains(NO_CONSUMER), "{consumer_only}");
    // Neutral: it claims nothing about dynamic emission, only that it was not resolved.
    assert!(consumer_only.contains("may publish"), "{consumer_only}");
    assert!(
        !consumer_only.contains("dynamic emitter"),
        "{consumer_only}"
    );

    let producer_only = impact(&state, json!({"target": "metrics:emitted"})).await;
    assert!(producer_only.contains(NO_CONSUMER), "{producer_only}");
    assert!(!producer_only.contains(NO_PRODUCER), "{producer_only}");
    assert!(
        producer_only.contains("a producer is indexed"),
        "{producer_only}"
    );
    assert!(
        producer_only.contains("registered dynamically"),
        "{producer_only}"
    );
    assert!(
        producer_only.contains("`topic_consumer`") && producer_only.contains("SETUP.md"),
        "{producer_only}"
    );
    assert!(
        !producer_only.to_lowercase().contains("authoritative"),
        "{producer_only}"
    );
}

#[tokio::test]
async fn resolved_pair_at_the_end_of_a_chain_gets_no_note() {
    let state = fixture();
    let text = impact(&state, json!({"target": "cache:purged"})).await;
    for marker in [NO_PRODUCER, NO_CONSUMER, DEPTH_LIMIT, CHAIN_END, "SETUP.md"] {
        assert!(!text.contains(marker), "unexpected {marker}: {text}");
    }
    assert!(
        text.contains("purgeCaches") && text.contains("listenCachePurged"),
        "{text}"
    );
}

#[tokio::test]
async fn notes_are_deterministic() {
    let state = fixture();
    let args = json!({"target": "user:deletion:requested", "depth": 3});
    let a = impact(&state, args.clone()).await;
    let b = impact(&state, args).await;
    assert_eq!(a, b);
}
