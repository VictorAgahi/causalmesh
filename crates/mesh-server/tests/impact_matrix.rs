#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Plan 4 step 4.6a: `analyze_impact` impact matrix on `examples/polyglot-shop`
//! (proto contracts, a Go and a Rust gRPC server, a TypeScript and a Go client).

use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config};
use mesh_server::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn polyglot_shop() -> Arc<AppState> {
    let dir = dunce::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/polyglot-shop"),
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

async fn matrix(state: &Arc<AppState>, args: Value) -> String {
    let res = ToolRegistry::call_tool("analyze_impact", args, state.clone())
        .await
        .expect("tool call");
    assert_eq!(res["isError"], json!(false), "{res}");
    res["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string()
}

/// The table rows, without the `#` column: `[scope, role, element, root, location, confidence, via]`.
fn rows(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| # "))
        .map(|l| {
            l.trim_matches('|')
                .split(" | ")
                .skip(1)
                .map(|c| c.trim().trim_matches('`').to_string())
                .collect()
        })
        .collect()
}

fn row(cells: [&str; 7]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

#[tokio::test]
async fn proto_method_change_lists_go_handler_and_ts_clients_by_scope() {
    let state = polyglot_shop();
    let text = matrix(&state, json!({"target": "ProcessPayment"})).await;
    assert!(
        text.contains("`shop.payment.v1/PaymentService.ProcessPayment` (`payment.proto:6`)"),
        "{text}"
    );
    assert!(
        text.contains("*Owner root(s): `services/payment-worker`"),
        "{text}"
    );
    assert_eq!(
        rows(&text),
        vec![
            // Service-level client (`getService('PaymentService')`): capped at heuristic.
            row([
                "EXTERNAL",
                "client",
                "constructor",
                "services/order-gateway",
                "order.service.ts:19",
                "heuristic",
                "PaymentService",
            ]),
            row([
                "EXTERNAL",
                "client",
                "rpc:ProcessPayment",
                "services/order-gateway",
                "order.service.ts:29",
                "heuristic",
                "PaymentService.ProcessPayment",
            ]),
            // Go server registered with `RegisterPaymentServiceServer`.
            row([
                "INTERNAL",
                "handler",
                "ProcessPayment",
                "services/payment-worker",
                "main.go:15",
                "heuristic",
                "PaymentService.ProcessPayment",
            ]),
        ],
        "{text}"
    );
}

#[tokio::test]
async fn proto_method_change_lists_rust_handler_and_go_client_by_scope() {
    let state = polyglot_shop();
    let text = matrix(&state, json!({"target": "InventoryService.ReserveStock"})).await;
    assert!(
        text.contains("*Owner root(s): `services/inventory-manager`"),
        "{text}"
    );
    assert_eq!(
        rows(&text),
        vec![
            // Go `pb.NewInventoryServiceClient(conn)` in payment-worker's `main`.
            row([
                "EXTERNAL",
                "client",
                "main",
                "services/payment-worker",
                "main.go:44",
                "heuristic",
                "InventoryService",
            ]),
            // tonic-style `impl InventoryService for InventoryServiceServer`.
            row([
                "INTERNAL",
                "handler",
                "reserve_stock",
                "services/inventory-manager",
                "src/main.rs:13",
                "heuristic",
                "InventoryService.ReserveStock",
            ]),
        ],
        "{text}"
    );
}

#[tokio::test]
async fn async_event_rows_are_classified_against_the_producer_root() {
    let state = polyglot_shop();
    let text = matrix(&state, json!({"target": "order-created-topic"})).await;
    assert!(
        text.contains("*Owner root(s): `services/order-gateway`"),
        "{text}"
    );
    let rows = rows(&text);
    let find = |role: &str, root: &str| {
        rows.iter()
            .find(|r| r[1] == role && r[3] == root)
            .unwrap_or_else(|| panic!("no {role} row in {root}: {text}"))
    };
    assert_eq!(find("producer", "services/order-gateway")[0], "INTERNAL");
    assert_eq!(find("consumer", "services/payment-worker")[0], "EXTERNAL");
    assert_eq!(find("consumer", "services/notification-hub")[0], "EXTERNAL");
    // The synthetic cross-repo topic hub has no root: never INTERNAL.
    assert_eq!(find("topic", "—")[0], "EXTERNAL");
    // No row appears twice.
    let mut unique = rows.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), rows.len(), "{text}");
}

#[tokio::test]
async fn pages_follow_the_offset_footer_and_cover_every_row_once() {
    let state = polyglot_shop();
    let full = rows(&matrix(&state, json!({"target": "order-created-topic"})).await);
    assert!(full.len() > 2);

    let mut paged = Vec::new();
    let mut offset = 0u32;
    loop {
        let text = matrix(
            &state,
            json!({"target": "order-created-topic", "limit": 2, "offset": offset}),
        )
        .await;
        let page = rows(&text);
        assert!(page.len() <= 2, "{text}");
        paged.extend(page);
        let Some(next) = text
            .split("`offset: ")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
        else {
            break;
        };
        offset = next.parse().expect("next offset");
    }
    assert_eq!(paged, full);

    let past = matrix(
        &state,
        json!({"target": "order-created-topic", "offset": 1000}),
    )
    .await;
    assert!(past.contains("No rows at `offset: 1000`"), "{past}");
}

#[tokio::test]
async fn unknown_argument_is_still_rejected() {
    let state = polyglot_shop();
    let res = ToolRegistry::call_tool(
        "analyze_impact",
        json!({"target": "ProcessPayment", "coverage": true}),
        state,
    )
    .await
    .expect("tool call");
    assert_eq!(res["isError"], json!(true), "{res}");
}

/// Latency budget of the milestone: < 100 ms per call on the fixture (median
/// of 51 calls through the registry, audit included). Release builds only:
/// a debug build measures the compiler, not the tool.
#[tokio::test]
async fn matrix_latency_on_the_fixture_is_under_100ms() {
    if cfg!(debug_assertions) {
        return;
    }
    let state = polyglot_shop();
    let first = matrix(&state, json!({"target": "ProcessPayment"})).await;
    let mut samples: Vec<Duration> = Vec::with_capacity(51);
    for _ in 0..51 {
        let start = Instant::now();
        let text = matrix(&state, json!({"target": "ProcessPayment"})).await;
        samples.push(start.elapsed());
        assert_eq!(text, first, "output must be identical across calls");
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    eprintln!(
        "analyze_impact(ProcessPayment) on polyglot-shop: median {median:?}, max {:?} over {} calls",
        samples[samples.len() - 1],
        samples.len()
    );
    assert!(median < Duration::from_millis(100), "median {median:?}");
}
