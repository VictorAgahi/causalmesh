#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! 7.1.7: `find_dependents` with `granularity: "package"` lists the dependent
//! PACKAGE with counts and a few representative locations, each with the
//! declaration's true kind, instead of one arbitrary "first symbol seen".

use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config};
use mesh_server::tools::ToolRegistry;
use serde_json::{json, Value};
use std::sync::Arc;

fn state_for(dir: &std::path::Path) -> Arc<AppState> {
    let config =
        Config::load_from_str("[workspace]\nname = \"tmp\"\nroots = [\".\"]\n").expect("config");
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

async fn call(state: &Arc<AppState>, args: Value) -> String {
    let res = ToolRegistry::call_tool("find_dependents", args, state.clone())
        .await
        .expect("tool call");
    assert_eq!(res["isError"], json!(false), "{res}");
    res["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string()
}

fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Under a directory named like test code: the root-relative judgement must
    // not classify the whole checkout as tests.
    let root = dunce::canonicalize(dir.path())
        .expect("canonical")
        .join("testing")
        .join("ws");
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }
    (dir, root)
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    workspace(&[
        ("svc-a/package.json", "{\"name\": \"svc-a\"}\n"),
        (
            "svc-a/src/a.service.ts",
            "import { pad } from 'left-pad';\nexport class AService {\n  run() { return pad('x'); }\n}\n",
        ),
        (
            "svc-a/src/consts.ts",
            "import { pad } from 'left-pad';\nexport const PARTICIPATION_BADGE_SLUGS = [pad('a')];\n",
        ),
        (
            "svc-a/src/types.ts",
            "import { pad } from 'left-pad';\nexport interface Thing {\n  v: typeof pad;\n}\n",
        ),
        (
            "svc-a/src/__generated__/schema.ts",
            "import { pad } from 'left-pad';\nexport const GENERATED_SCHEMA = [pad('g')];\n",
        ),
        (
            "svc-a/src/a.spec.ts",
            "import { pad } from 'left-pad';\nexport class ASpec { run() { return pad('t'); } }\n",
        ),
        ("svc-b/package.json", "{\"name\": \"svc-b\"}\n"),
        (
            "svc-b/src/test-utils/factory.ts",
            "import { pad } from 'left-pad';\nexport const makeB = () => pad('b');\n",
        ),
        ("svc-c/package.json", "{\"name\": \"svc-c\"}\n"),
        (
            "svc-c/src/c.ts",
            "import { pad } from 'left-pad';\nexport enum Mode { On, Off }\nexport function useC() { return pad('c'); }\n",
        ),
    ])
}

#[tokio::test]
async fn package_mode_lists_packages_with_counts_and_honest_kinds() {
    let (_guard, root) = fixture();
    let state = state_for(&root);
    let out = call(
        &state,
        json!({"target": "left-pad", "granularity": "package"}),
    )
    .await;

    // Header counts packages, the test-only package is left out and counted.
    assert!(out.contains("2 dependent package(s)"), "{out}");
    assert!(
        out.contains("1 more package(s) in test files left out"),
        "{out}"
    );
    assert!(!out.contains("svc-b"), "{out}");

    // The package is the unit, with file and symbol counts (the spec file is
    // test code, left out; the generated file still counts).
    assert!(out.contains("`svc-a`"), "{out}");
    assert!(out.contains("4 file(s), 4 symbol(s) use it"), "{out}");
    assert!(out.contains("`svc-c`"), "{out}");

    // Three representatives at most, each in a different file; generated code
    // ranks below hand-written code and is not shown while three exist.
    let a_block: String = out
        .lines()
        .skip_while(|l| !l.contains("`svc-a`"))
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        a_block.lines().filter(|l| l.starts_with("- `")).count(),
        3,
        "{a_block}"
    );
    assert!(!a_block.contains("__generated__"), "{a_block}");
    assert!(!a_block.contains("a.spec.ts"), "{a_block}");
    assert!(a_block.contains("and 1 more file(s)"), "{a_block}");

    // True kinds, never the generic fallback label.
    assert!(
        a_block.contains("`PARTICIPATION_BADGE_SLUGS` (const)"),
        "{a_block}"
    );
    assert!(a_block.contains("`Thing` (interface)"), "{a_block}");
    assert!(a_block.contains("`AService` (class)"), "{a_block}");
    assert!(!out.contains("ServiceClass"), "{out}");
    assert!(
        out.contains("(function)") || out.contains("(enum)"),
        "{out}"
    );

    // Locations are root-relative file:line.
    assert!(out.contains("svc-a/src/consts.ts:2"), "{out}");
    assert!(!out.contains("/testing/ws/"), "{out}");
}

#[tokio::test]
async fn package_mode_is_deterministic_and_include_tests_lists_test_only_packages() {
    let (_guard, root) = fixture();
    let state = state_for(&root);
    let args = json!({"target": "left-pad", "granularity": "package"});
    let first = call(&state, args.clone()).await;
    for _ in 0..5 {
        assert_eq!(call(&state, args.clone()).await, first);
    }
    // Packages are ordered by files desc: svc-a (4), then svc-c (1).
    let a = first.find("`svc-a`").expect("svc-a");
    let c = first.find("`svc-c`").expect("svc-c");
    assert!(a < c, "{first}");

    let with_tests = call(
        &state,
        json!({"target": "left-pad", "granularity": "package", "include_tests": true}),
    )
    .await;
    assert!(
        with_tests.contains("3 dependent package(s)"),
        "{with_tests}"
    );
    assert!(with_tests.contains("`svc-b`"), "{with_tests}");
    assert!(!with_tests.contains("left out"), "{with_tests}");
    // Test code ranks last inside a package that also has production code.
    let a_block: String = with_tests
        .lines()
        .skip_while(|l| !l.contains("`svc-a`"))
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!a_block.contains("a.spec.ts"), "{a_block}");
}

#[tokio::test]
async fn package_mode_pages_and_reports_the_scanned_files_honestly() {
    let (_guard, root) = fixture();
    let state = state_for(&root);
    let page = call(
        &state,
        json!({"target": "left-pad", "granularity": "package", "limit": 1, "offset": 1}),
    )
    .await;
    assert!(page.contains("`svc-c`"), "{page}");
    assert!(!page.contains("`svc-a`"), "{page}");
    assert!(page.contains("[2] `svc-c`"), "{page}");
    assert!(page.contains("showing 2-2"), "{page}");

    // No dependents: the scanned-files figure is the indexed-file count that
    // `doctor` reports, not the number of contract-bearing files.
    let none = call(
        &state,
        json!({"target": "no-such-package", "granularity": "package"}),
    )
    .await;
    let indexed = state.snapshot().health.files_indexed;
    assert!(indexed > 0);
    assert!(
        none.contains(&format!("{indexed} indexed source files scanned")),
        "{none}"
    );
}
