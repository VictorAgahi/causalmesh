#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Plan 4 step 4.4 exit criteria for the per-workspace persistent index cache:
//!
//! - 4 processes indexing 4 workspaces at the same time hit 0 SQLite errors
//!   (`SQLITE_BUSY` included), each writing its own
//!   `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db`;
//! - a daemon-style `AppState` reloading 30 times without restarting keeps its
//!   cache under quota (the reload path writes to the cache and triggers the
//!   quota check).
//!
//! The quota-eviction unit criterion (10 MB test quota → ≤ 80 %) lives next to
//! the implementation, in `mesh_core::index_cache`'s tests.

use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine, Config, PersistentIndexCache};
use mesh_server::WorkspaceIndexer;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "MESH_TEST_CACHE_CHILD_WS";
const GO_ENV: &str = "MESH_TEST_CACHE_CHILD_GO";

fn rust_file(i: usize, round: usize) -> String {
    format!(
        "// round {round}\n\
         pub struct Service{i} {{ pub name: String, pub round: u32 }}\n\n\
         impl Service{i} {{\n\
         \x20   pub fn new(name: String) -> Self {{ Self {{ name, round: {round} }} }}\n\
         \x20   pub fn handle_{i}_{round}(&self, input: &str) -> String {{ format!(\"{{}}::{{}}\", self.name, input) }}\n\
         \x20   pub fn describe_{i}(&self) -> usize {{ self.name.len() + {round} }}\n\
         }}\n"
    )
}

fn write_workspace(root: &Path, files: usize, round: usize) {
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    for i in 0..files {
        std::fs::write(
            root.join("src").join(format!("f{i}.rs")),
            rust_file(i, round),
        )
        .expect("write");
    }
}

fn config_for(root: &Path) -> Config {
    let mut cfg = Config::load_from_str(&format!(
        "[workspace]\nname = \"cache-quota\"\nroots = [{:?}]\n",
        root.display().to_string()
    ))
    .expect("config");
    cfg.resolve_workspace_root(root);
    cfg
}

/// Child half of `four_processes_indexing_four_workspaces_hit_zero_sqlite_errors`:
/// only does anything when spawned by it (the env var names its workspace).
#[test]
#[ignore = "spawned as a child process by four_processes_indexing_four_workspaces_hit_zero_sqlite_errors"]
fn child_index_one_workspace() {
    let Some(ws) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let ws = PathBuf::from(ws);
    let go = PathBuf::from(std::env::var_os(GO_ENV).expect("go file"));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !go.exists() {
        assert!(Instant::now() < deadline, "parent never signalled go");
        std::thread::sleep(Duration::from_millis(5));
    }

    let config = config_for(&ws);
    let roots = WorkspaceIndexer::resolve_roots(&config, &ws);
    let cache = PersistentIndexCache::open_for_workspace(&ws, 2048).expect("open cache");
    // Cold, warm, then a rewrite of every file: three scans, the first and last writing
    // every entry in one transaction each.
    let cold = WorkspaceIndexer::build_snapshot(&config, &roots, None, None, Some(&cache));
    let warm = WorkspaceIndexer::build_snapshot(&config, &roots, None, None, Some(&cache));
    assert_eq!(cold.fingerprint(), warm.fingerprint());
    write_workspace(&ws, 300, 1);
    WorkspaceIndexer::build_snapshot(&config, &roots, None, None, Some(&cache));
    let stats = cache.stats();
    assert_eq!(
        stats.errors, 0,
        "SQLite errors (SQLITE_BUSY included): {stats:?}"
    );
    assert!(stats.hits >= 300, "warm scan must hit the cache: {stats:?}");
}

#[test]
fn four_processes_indexing_four_workspaces_hit_zero_sqlite_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = dunce::canonicalize(tmp.path()).expect("canon");
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let go = base.join("go");
    let exe = std::env::current_exe().expect("test exe");

    let workspaces: Vec<PathBuf> = (0..4).map(|n| base.join(format!("ws{n}"))).collect();
    for ws in &workspaces {
        write_workspace(ws, 300, 0);
    }
    let children: Vec<_> = workspaces
        .iter()
        .map(|ws| {
            Command::new(&exe)
                .args([
                    "child_index_one_workspace",
                    "--exact",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, ws)
                .env(GO_ENV, &go)
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn child")
        })
        .collect();
    std::fs::write(&go, b"go").expect("go");
    for (child, ws) in children.into_iter().zip(&workspaces) {
        let out = child.wait_with_output().expect("wait child");
        assert!(
            out.status.success(),
            "child for {} failed:\n{}\n{}",
            ws.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // One database per workspace, named by the same id as the workspace's socket.
    let cache_root = home.join(".cache").join("mesh-mcp");
    for ws in &workspaces {
        let id = mesh_core::workspace_id(ws);
        let db = cache_root
            .join("workspaces")
            .join(&id)
            .join("index-cache.db");
        assert!(db.is_file(), "missing per-workspace cache {}", db.display());
    }
    assert!(
        !cache_root.join("index-cache.db").exists(),
        "the legacy machine-wide cache must not be created any more"
    );
}

#[test]
fn daemon_reloading_30_times_stays_under_quota() {
    const QUOTA: u64 = 1024 * 1024;
    const FILES: usize = 400;
    const CHANGED_PER_RELOAD: usize = 150;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(tmp.path()).expect("canon").join("ws");
    write_workspace(&root, FILES, 0);
    let config = config_for(&root);
    let roots = WorkspaceIndexer::resolve_roots(&config, &root);
    let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
    let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
    let state = Arc::new(AppState::new(config, roots, audit, rescan));
    let db = tmp.path().join("cache").join("index-cache.db");
    assert!(state
        .index_cache
        .set(PersistentIndexCache::open(db, QUOTA).expect("open cache"))
        .is_ok());
    let cache = state.index_cache.get().expect("attached");

    let snapshot = {
        let mut vfs = state.vfs.lock().expect("vfs");
        WorkspaceIndexer::build_snapshot(
            &state.config,
            &state.allowed_roots,
            None,
            Some(&mut vfs),
            Some(cache),
        )
    };
    state.install_snapshot(snapshot);
    assert!(cache.size_bytes() <= QUOTA);

    for round in 1..=30 {
        // A different slice of files each round, rewritten with never-seen content, so
        // every reload writes > 100 new entries.
        for k in 0..CHANGED_PER_RELOAD {
            let i = (round * 37 + k) % FILES;
            std::fs::write(
                root.join("src").join(format!("f{i}.rs")),
                rust_file(i, round),
            )
            .expect("rewrite");
        }
        WorkspaceIndexer::reload(&state);
        let size = cache.size_bytes();
        assert!(
            size <= QUOTA,
            "reload {round}: cache {size} bytes exceeds quota {QUOTA}"
        );
    }
    let stats = cache.stats();
    assert_eq!(stats.errors, 0, "{stats:?}");
    assert!(
        stats.evicted > 0,
        "30 reloads must have pushed the cache past its quota at least once: {stats:?}"
    );
}
