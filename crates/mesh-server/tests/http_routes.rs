#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! 7.1.5: HTTP routes are reachable by route. `smart_search` matches the
//! `HttpEndpoint` signature, `analyze_impact` resolves `VERB /path`, and a
//! zoomed `visualize_mesh` lists the service's routes, on
//! `examples/http-routes-fixture` (two NestJS roots, a test-only controller,
//! `http_prefix = "/api/v1"`).

use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config};
use mesh_server::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

fn fixture(prefix: Option<&str>) -> Arc<AppState> {
    let dir = dunce::canonicalize(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/http-routes-fixture"),
    )
    .expect("canonicalize fixture");
    let config = Config::load_from_file(&dir.join("mesh-mcp.toml")).expect("fixture config");
    build_state(config, &dir, prefix)
}

/// A one-off workspace rooted at `dir` (roots relative to it).
fn state_for(dir: &std::path::Path, roots: &[&str], prefix: Option<&str>) -> Arc<AppState> {
    let list: Vec<String> = roots.iter().map(|r| format!("\"{r}\"")).collect();
    let config = Config::load_from_str(&format!(
        "[workspace]\nname = \"tmp\"\nroots = [{}]\n",
        list.join(", ")
    ))
    .expect("config");
    build_state(config, dir, prefix)
}

fn build_state(mut config: Config, dir: &std::path::Path, prefix: Option<&str>) -> Arc<AppState> {
    config.workspace.http_prefix = prefix.map(str::to_string);
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

async fn call(state: &Arc<AppState>, tool: &str, args: Value) -> String {
    let res = ToolRegistry::call_tool(tool, args, state.clone())
        .await
        .expect("tool call");
    assert_eq!(res["isError"], json!(false), "{res}");
    res["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string()
}

#[test]
fn config_accepts_http_prefix_and_rejects_unknown_keys() {
    let ok = Config::load_from_str(
        "[workspace]\nname = \"w\"\nroots = [\".\"]\nhttp_prefix = \"/api/v1\"\n",
    )
    .expect("http_prefix is a workspace key");
    assert_eq!(ok.workspace.http_prefix.as_deref(), Some("/api/v1"));
    assert!(Config::load_from_str(
        "[workspace]\nname = \"w\"\nroots = [\".\"]\nhttp_prefixes = \"/api\"\n"
    )
    .is_err());
}

#[tokio::test]
async fn smart_search_finds_a_route_in_any_parameter_spelling() {
    let state = fixture(Some("/api/v1"));
    for query in [
        "PATCH /events/:id/state",
        "patch /events/{id}/state",
        "Patch /events/<id>/state",
        "PATCH /events/[id]/state",
        "PATCH /events/:eventId/state",
        "PATCH /api/v1/events/:id/state",
        "/events/:id/state",
    ] {
        let text = call(&state, "smart_search", json!({"query": query})).await;
        assert!(text.contains("changeEventState"), "{query}\n{text}");
        assert!(
            text.contains("PATCH /api/v1/events/:id/state"),
            "{query}\n{text}"
        );
        assert!(text.contains("events.controller.ts"), "{query}\n{text}");
        assert!(text.contains("[root: gateway]"), "{query}\n{text}");
    }
}

#[tokio::test]
async fn two_routes_differing_only_in_verb_are_told_apart() {
    let state = fixture(None);
    let patch = call(
        &state,
        "smart_search",
        json!({"query": "PATCH /events/:id/state"}),
    )
    .await;
    assert!(patch.contains("changeEventState"), "{patch}");
    assert!(!patch.contains("replaceEventState"), "{patch}");
    let put = call(
        &state,
        "smart_search",
        json!({"query": "put /events/:id/state"}),
    )
    .await;
    assert!(put.contains("replaceEventState"), "{put}");
    assert!(!put.contains("changeEventState"), "{put}");
    // Without a verb, both routes of the path are listed.
    let both = call(
        &state,
        "smart_search",
        json!({"query": "/events/:id/state"}),
    )
    .await;
    assert!(
        both.contains("changeEventState") && both.contains("replaceEventState"),
        "{both}"
    );
}

#[tokio::test]
async fn partial_paths_and_method_plus_fragment_match() {
    let state = fixture(None);
    let text = call(&state, "smart_search", json!({"query": "GET /attendees"})).await;
    assert!(text.contains("listAttendees"), "{text}");
    assert!(!text.contains("getEvent"), "{text}");
    let text = call(&state, "smart_search", json!({"query": "/invoices"})).await;
    assert!(
        text.contains("getInvoice") && text.contains("payInvoice"),
        "{text}"
    );
    assert!(text.contains("[root: billing]"), "{text}");
}

#[tokio::test]
async fn symbol_name_search_is_unchanged() {
    let state = fixture(None);
    let text = call(&state, "smart_search", json!({"query": "changeEventState"})).await;
    assert!(text.contains("events.controller.ts"), "{text}");
    assert!(!text.contains("Route match"), "{text}");
}

#[tokio::test]
async fn test_only_routes_are_excluded_unless_asked() {
    let state = fixture(None);
    let text = call(
        &state,
        "smart_search",
        json!({"query": "GET /events/only-in-tests/special"}),
    )
    .await;
    assert!(!text.contains("testOnlyRoute"), "{text}");
    assert!(text.contains("match only in test files"), "{text}");
    let text = call(
        &state,
        "smart_search",
        json!({"query": "GET /events/only-in-tests/special", "include_tests": true}),
    )
    .await;
    assert!(text.contains("testOnlyRoute"), "{text}");
    // The production route keeps its own listing; the fake controller is only counted.
    let text = call(
        &state,
        "smart_search",
        json!({"query": "PATCH /events/:id/state"}),
    )
    .await;
    assert!(text.contains("changeEventState"), "{text}");
    assert!(!text.contains("fakeChangeEventState"), "{text}");
    assert!(text.contains("1 route(s) in test files left out"), "{text}");
}

#[tokio::test]
async fn analyze_impact_resolves_a_route_to_its_handler() {
    let state = fixture(Some("/api/v1"));
    let text = call(
        &state,
        "analyze_impact",
        json!({"target": "PATCH /events/:eventId/state"}),
    )
    .await;
    assert!(text.contains("Impact for HTTP route"), "{text}");
    assert!(text.contains("changeEventState"), "{text}");
    assert!(text.contains("events.controller.ts:"), "{text}");
    assert!(text.contains("PATCH /api/v1/events/:id/state"), "{text}");
    assert!(text.contains("Scope: the handler only"), "{text}");
    assert!(!text.contains("replaceEventState"), "{text}");
    assert!(!text.contains("fakeChangeEventState"), "{text}");
}

#[tokio::test]
async fn analyze_impact_no_match_suggests_nearest_routes() {
    let state = fixture(None);
    let text = call(
        &state,
        "analyze_impact",
        json!({"target": "PATCH /events/:id/status"}),
    )
    .await;
    assert!(text.contains("No HTTP route matches"), "{text}");
    assert!(!text.contains("Pass an exact proto method"), "{text}");
    assert!(text.contains("Nearest routes"), "{text}");
    // Same prefix (`/events/...`) comes before other repos' routes.
    assert!(text.contains("`PATCH /events/:id/state`"), "{text}");
    let first_events = text.find("/events/").expect("events route listed");
    if let Some(invoices) = text.find("/invoices/") {
        assert!(first_events < invoices, "{text}");
    }
}

#[tokio::test]
async fn analyze_impact_non_route_target_keeps_the_generic_path() {
    let state = fixture(None);
    let text = call(&state, "analyze_impact", json!({"target": "NoSuchEvent"})).await;
    assert!(text.contains("Impact Matrix"), "{text}");
    // A verb without a leading slash is not a route target.
    let text = call(&state, "analyze_impact", json!({"target": "GET events"})).await;
    assert!(text.contains("Impact Matrix"), "{text}");
}

#[tokio::test]
async fn visualize_mesh_service_zoom_lists_the_routes_and_http_count() {
    let state = fixture(Some("/api/v1"));
    let text = call(&state, "visualize_mesh", json!({"service": "gateway"})).await;
    assert!(text.contains("HTTP routes (6)"), "{text}");
    assert!(
        text.contains("by kind:") && text.contains("6 HTTP"),
        "{text}"
    );
    assert!(
        text.contains("PATCH /api/v1/events/:id/state` -> `changeEventState`"),
        "{text}"
    );
    assert!(
        text.contains("GET /api/v1/events/:eventId/attendees"),
        "{text}"
    );
    let routes = text.split("HTTP routes (").nth(1).expect("routes section");
    assert!(!routes.contains("testOnlyRoute"), "{text}");
    assert!(!routes.contains("fakeChangeEventState"), "{text}");
    assert!(
        routes.contains("2 route(s) in test files not listed"),
        "{text}"
    );
}

#[tokio::test]
async fn visualize_mesh_whole_mesh_keeps_http_in_the_service_caption() {
    let state = fixture(None);
    let text = call(&state, "visualize_mesh", json!({})).await;
    assert!(text.contains("HTTP"), "{text}");
}

#[tokio::test]
async fn plain_verb_word_queries_are_symbol_queries() {
    let state = fixture(None);
    for query in ["get user", "delete account", "post message"] {
        let text = call(&state, "smart_search", json!({"query": query})).await;
        assert!(!text.contains("Nearest routes"), "{query}\n{text}");
        assert!(!text.contains("No route matches"), "{query}\n{text}");
    }
    // Still a route shape without a verb or with a slash.
    let text = call(&state, "smart_search", json!({"query": "get /invoices"})).await;
    assert!(text.contains("getInvoice"), "{text}");
}

#[tokio::test]
async fn a_symbol_match_wins_over_a_fragment_only_route_hit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path())
        .expect("canonical")
        .join("svc");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/a.controller.ts"),
        "import { Controller, Get } from '@nestjs/common';\n@Controller('events')\nexport class A {\n  @Get(':id/state')\n  stateOf() { return 1; }\n}\n",
    )
    .expect("write");
    // A queue named like a route fragment.
    std::fs::write(
        root.join("src/b.consumer.ts"),
        "import { EventPattern } from '@nestjs/microservices';\nexport class B {\n  @EventPattern('/state')\n  onState() { return 2; }\n}\n",
    )
    .expect("write");
    let state = state_for(&root, &["."], None);
    let text = call(&state, "smart_search", json!({"query": "/state"})).await;
    let symbol_hit = text.contains("b.consumer.ts");
    let route_header = text.contains("-> stateOf") || text.contains("->  stateOf");
    // Whatever the indexer names that topic, the two must not be mixed up: either
    // the symbol is shown (and the weak route fragment is not), or no symbol
    // exists and the fragment route is shown.
    assert!(symbol_hit != route_header, "{text}");
    let strong = call(
        &state,
        "smart_search",
        json!({"query": "GET /events/:id/state"}),
    )
    .await;
    assert!(strong.contains("stateOf"), "{strong}");
}

#[tokio::test]
async fn a_root_under_a_test_named_directory_keeps_its_routes() {
    for outer in ["testing", "e2e"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path())
            .expect("canonical")
            .join(outer)
            .join("gw");
        std::fs::create_dir_all(root.join("src/__test-utils__")).expect("mkdir");
        std::fs::write(
            root.join("src/real.controller.ts"),
            "import { Controller, Get } from '@nestjs/common';\n@Controller('things')\nexport class Real {\n  @Get(':id')\n  realThing() { return 1; }\n}\n",
        )
        .expect("write");
        std::fs::write(
            root.join("src/__test-utils__/fake.controller.ts"),
            "import { Controller, Get } from '@nestjs/common';\n@Controller('things')\nexport class Fake {\n  @Get('fake')\n  fakeThing() { return 1; }\n}\n",
        )
        .expect("write");
        let state = state_for(&root, &["."], None);
        let text = call(&state, "smart_search", json!({"query": "GET /things/:id"})).await;
        assert!(text.contains("realThing"), "{outer}\n{text}");
        let text = call(&state, "smart_search", json!({"query": "GET /things/fake"})).await;
        assert!(!text.contains("fakeThing"), "{outer}\n{text}");
        let text = call(
            &state,
            "smart_search",
            json!({"query": "GET /things/fake", "include_tests": true}),
        )
        .await;
        assert!(text.contains("fakeThing"), "{outer}\n{text}");
        let text = call(
            &state,
            "analyze_impact",
            json!({"target": "GET /things/:id"}),
        )
        .await;
        assert!(text.contains("realThing"), "{outer}\n{text}");
    }
}
