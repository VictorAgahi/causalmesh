use crate::indexer::WorkspaceIndexer;
use mesh_core::{AppState, ReloadFn};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// High-level FileWatcherService binding the workspace indexer to the in-kernel core watcher per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: std::time::Duration =
        mesh_core::FileWatcherService::DEBOUNCE_INTERVAL;

    /// Spawns the debounced file watcher actor wired to the differential reload.
    pub fn spawn(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        let reload: ReloadFn = Arc::new(WorkspaceIndexer::reload_paths);
        mesh_core::FileWatcherService::spawn(state, cancel_token, reload)
    }

    #[inline]
    pub fn is_relevant_path(path: &std::path::Path) -> bool {
        mesh_core::FileWatcherService::is_relevant_path(path)
    }

    /// Runs one differential reload synchronously on the calling thread.
    pub fn execute_reload_sync(state: &AppState) {
        WorkspaceIndexer::reload(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn test_relevant_paths_filter() {
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "src/main.rs"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "api/auth.proto"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "service.ts"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(".git/HEAD")));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            ".git/refs/heads/main"
        )));

        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "target/debug/app"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "node_modules/pkg/index.js"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            ".git/objects/4b/825dc"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "src/main.rs.swp"
        )));
    }

    #[test]
    fn test_file_watcher_live_reload() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let allowed_root = dunce::canonicalize(temp_dir.path()).expect("canonical temp dir");

        let proto_file = allowed_root.join("test.proto");
        std::fs::write(
            &proto_file,
            "syntax = \"proto3\"; package test.v1; service ServiceA { rpc CallA (Req) returns (Resp); }",
        )
        .expect("write proto 1");

        let default_cfg = mesh_core::Config::load_from_str(
            r#"
[workspace]
name = "test-watcher"
version = "2.9.0"
roots = ["."]
"#,
        )
        .expect("load config");

        let audit = Arc::new(
            mesh_core::AuditLogger::new(Some(temp_dir.path().join("audit.db")))
                .expect("init audit"),
        );
        let rescan = Arc::new(mesh_core::BackgroundRescanEngine::new().expect("init rescan"));

        let state = Arc::new(AppState::new(
            default_cfg,
            vec![allowed_root.clone()],
            audit,
            rescan,
        ));

        // Initial sync reload
        FileWatcherService::execute_reload_sync(&state);
        assert_eq!(state.snapshot().contract_graph.node_count(), 2);

        // Spawn live watcher
        let cancel_token = CancellationToken::new();
        let watcher_handle =
            FileWatcherService::spawn(state.clone(), cancel_token.clone()).expect("spawn watcher");

        // Give the OS watcher a short moment to establish watches
        std::thread::sleep(Duration::from_millis(100));

        // Add a second service in a new proto file
        let proto2_file = allowed_root.join("test2.proto");
        std::fs::write(
            &proto2_file,
            "syntax = \"proto3\"; package test.v2; service ServiceB { rpc CallB (Req) returns (Resp); }",
        )
        .expect("write proto 2");

        // Wait for debounce (150ms) + background Rayon execution (max 2500ms)
        let mut reloaded = false;
        for _ in 0..25 {
            std::thread::sleep(Duration::from_millis(100));
            if state.snapshot().contract_graph.node_count() > 2 {
                reloaded = true;
                break;
            }
        }

        cancel_token.cancel();
        let _ = watcher_handle.join();

        assert!(
            reloaded,
            "ContractGraph in AppState must be automatically reloaded by FileWatcherService upon file change! Expected > 2 nodes, got: {}",
            state.snapshot().contract_graph.node_count()
        );
        assert_eq!(state.snapshot().contract_graph.node_count(), 4);
    }

    /// Plan 4 step 4.2, end to end on the real OS watcher: while `.git/index.lock` exists, file
    /// changes are held (no generation installed, tools answer from the old one with a note);
    /// once the lock is gone and `HEAD` has settled, everything is reloaded in exactly one new
    /// generation. The Git directory is synthetic (no `git` binary needed) but laid out exactly
    /// as Git lays it out.
    #[test]
    fn git_operation_holds_reloads_until_lock_released_then_reloads_once() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = dunce::canonicalize(temp_dir.path()).expect("canonical temp dir");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("mkdir .git");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        std::fs::write(
            root.join("a.proto"),
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); }",
        )
        .expect("write a");

        let cfg = mesh_core::Config::load_from_str(
            "[workspace]\nname = \"t-git\"\nversion = \"0\"\nroots = [\".\"]\n",
        )
        .expect("config");
        let audit = Arc::new(mesh_core::AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(mesh_core::BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, vec![root.clone()], audit, rescan));
        FileWatcherService::execute_reload_sync(&state);
        let gen0 = state.snapshot().generation;
        assert_eq!(state.snapshot().contract_graph.node_count(), 2);

        let cancel_token = CancellationToken::new();
        let handle =
            FileWatcherService::spawn(state.clone(), cancel_token.clone()).expect("spawn watcher");
        std::thread::sleep(Duration::from_millis(200));

        // "git checkout" starts: lock first, then the working tree is rewritten.
        std::fs::write(root.join(".git/index.lock"), "").expect("lock");
        std::thread::sleep(Duration::from_millis(100));
        for name in ["b", "c"] {
            std::fs::write(
                root.join(format!("{name}.proto")),
                format!("syntax = \"proto3\"; package {name}; service S{name} {{ rpc Y (R) returns (S); }}"),
            )
            .expect("write proto");
        }

        // Well past debounce + settle: still nothing installed, and tools say why.
        let mut noted = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if mesh_core::FileWatcherService::git_operation_note(&state).is_some() {
                noted = true;
            }
        }
        assert!(
            noted,
            "the hold must be visible to tools while the lock exists"
        );
        assert_eq!(
            state.snapshot().generation,
            gen0,
            "no generation may be installed while index.lock exists"
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let answer = rt
            .block_on(crate::tools::ToolRegistry::call_tool(
                "search_docs",
                serde_json::json!({"query": "anything"}),
                state.clone(),
            ))
            .expect("tool call");
        let text = answer["content"][0]["text"].as_str().unwrap_or_default();
        assert!(
            text.contains("Git operation in progress")
                && text.contains(&format!("generation {gen0}")),
            "tool text must carry the note: {text}"
        );

        // Operation ends: lock removed, then HEAD rewritten (checkout order).
        std::fs::remove_file(root.join(".git/index.lock")).expect("unlock");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature\n").expect("HEAD");

        let mut reloaded = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if state.snapshot().generation > gen0 {
                reloaded = true;
                break;
            }
        }
        // Let any straggler reload land before counting.
        std::thread::sleep(Duration::from_millis(1500));
        cancel_token.cancel();
        let _ = handle.join();

        assert!(
            reloaded,
            "the held changes must be reloaded once the operation ends"
        );
        assert_eq!(
            state.snapshot().generation,
            gen0 + 1,
            "exactly one grouped reload after the operation"
        );
        assert_eq!(state.snapshot().contract_graph.node_count(), 6);
        assert_eq!(
            mesh_core::FileWatcherService::git_operation_note(&state),
            None
        );
    }

    /// On macOS (Plan 4 step 4.2) the directory *is* covered by the root's recursive FSEvents
    /// stream, and the same guarantee comes from the in-memory event filter instead.
    ///
    /// P2 step 3.3: registration-time filtering must mean an excluded directory never even gets
    /// a watch — not just that its events are filtered downstream. `node_modules` is in the
    /// default `exclude_patterns`, so a file written there after the watcher has started must
    /// never trigger a reload at all (unlike `test_file_watcher_live_reload`'s file written
    /// directly under the watched root, which does).
    #[test]
    fn excluded_directory_never_triggers_a_reload() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let allowed_root = dunce::canonicalize(temp_dir.path()).expect("canonical temp dir");
        std::fs::create_dir_all(allowed_root.join("node_modules/pkg")).expect("mkdir");

        let default_cfg = mesh_core::Config::load_from_str(
            r#"
[workspace]
name = "test-watcher-exclude"
version = "2.9.0"
roots = ["."]
"#,
        )
        .expect("load config");
        let audit = Arc::new(
            mesh_core::AuditLogger::new(Some(temp_dir.path().join("audit.db")))
                .expect("init audit"),
        );
        let rescan = Arc::new(mesh_core::BackgroundRescanEngine::new().expect("init rescan"));
        let state = Arc::new(AppState::new(
            default_cfg,
            vec![allowed_root.clone()],
            audit,
            rescan,
        ));

        FileWatcherService::execute_reload_sync(&state);
        assert_eq!(state.snapshot().contract_graph.node_count(), 0);

        let cancel_token = CancellationToken::new();
        let watcher_handle =
            FileWatcherService::spawn(state.clone(), cancel_token.clone()).expect("spawn watcher");
        std::thread::sleep(Duration::from_millis(100));

        // A real declared symbol, so if this *did* trigger a reload it would be unmistakable —
        // not caught by exclude_patterns after the fact, but never watched at all beforehand.
        std::fs::write(
            allowed_root.join("node_modules/pkg/index.ts"),
            "export class ShouldNeverBeIndexed {}",
        )
        .expect("write excluded file");

        std::thread::sleep(Duration::from_millis(800));
        cancel_token.cancel();
        let _ = watcher_handle.join();

        assert_eq!(
            state.snapshot().contract_graph.node_count(),
            0,
            "a file inside an excluded directory must never trigger a reload — it should never \
             have been watched in the first place"
        );
    }

    /// P2 step 3.3: per-directory registration doesn't automatically track new subdirectories
    /// the way a single recursive watch did — `FileWatcherService::spawn`'s event loop must
    /// dynamically register a watch on a directory created after startup, or a whole new
    /// service directory added post-boot (e.g. `git checkout` of a branch that adds one) would
    /// silently never be watched.
    ///
    /// Since Plan 4 step 4.2 macOS no longer registers per directory (one recursive FSEvents
    /// stream per root covers new directories natively; this test passes there in ~1s), so
    /// the dynamic path exercised here is the Linux/Windows one.
    ///
    /// `#[ignore]`d rather than run by default: this exercises the *real* OS watcher, and on
    /// macOS (before 4.2) a dynamic `.watch()` call measurably took over 11 seconds under this machine's own
    /// test-suite load (FSEvents restarts its whole stream per call — see `FileWatcherService::
    /// spawn`'s doc). The write-retry loop below already budgets 20s specifically to absorb that,
    /// but `cargo test --workspace`'s full parallel run adds enough additional CPU contention
    /// that the same 20s budget isn't reliably enough — this became flaky, not wrong, when run
    /// alongside every other test. `plan_watch_dirs_*` in `mesh-core::crawler` covers the same
    /// dynamic-registration *decision* (which directories, respecting excludes) synchronously
    /// and deterministically; this test is for occasionally re-confirming the real end-to-end
    /// wall-clock behavior by hand (`cargo test -p mesh-server --lib -- --ignored
    /// new_subdirectory_created_after_startup_is_still_watched`), not for the default gate.
    #[test]
    #[ignore]
    fn new_subdirectory_created_after_startup_is_still_watched() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let allowed_root = dunce::canonicalize(temp_dir.path()).expect("canonical temp dir");

        let default_cfg = mesh_core::Config::load_from_str(
            r#"
[workspace]
name = "test-watcher-new-dir"
version = "2.9.0"
roots = ["."]
"#,
        )
        .expect("load config");
        let audit = Arc::new(
            mesh_core::AuditLogger::new(Some(temp_dir.path().join("audit.db")))
                .expect("init audit"),
        );
        let rescan = Arc::new(mesh_core::BackgroundRescanEngine::new().expect("init rescan"));
        let state = Arc::new(AppState::new(
            default_cfg,
            vec![allowed_root.clone()],
            audit,
            rescan,
        ));

        FileWatcherService::execute_reload_sync(&state);
        assert_eq!(state.snapshot().contract_graph.node_count(), 0);

        let cancel_token = CancellationToken::new();
        let watcher_handle =
            FileWatcherService::spawn(state.clone(), cancel_token.clone()).expect("spawn watcher");
        std::thread::sleep(Duration::from_millis(100));

        // This directory did not exist at `spawn` time, so it had no watch registered on it
        // initially — only the dynamic re-registration path (triggered by the mkdir event
        // itself) can make a write inside it observed at all.
        let new_dir = allowed_root.join("new_service");
        std::fs::create_dir_all(&new_dir).expect("mkdir new_service");

        // Dynamic registration is deliberately deferred onto a detached background thread (see
        // `FileWatcherService::spawn`'s doc — NOT `state.rescan`'s pool, which is intentionally
        // avoided since it also runs real reload jobs) precisely because a native watcher's
        // `.watch()` call is not cheap on every platform: macOS's FSEvents backend stops and
        // restarts its whole event stream per call, measured on this machine at 11+ seconds
        // under load. A
        // write made before that registration lands is permanently missed (no retroactive
        // delivery), so this test keeps writing until either the watch catches up and the write
        // is observed, or a generous budget elapses — proving eventual coverage, not instant
        // coverage, which is what step 3.3 actually promises for a directory created after boot.
        let mut reloaded = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(500));
            std::fs::write(
                new_dir.join("service.proto"),
                "syntax = \"proto3\"; package new; service New { rpc Call (Req) returns (Resp); }",
            )
            .expect("write proto in new dir");
            if state.snapshot().contract_graph.node_count() > 0 {
                reloaded = true;
                break;
            }
        }

        cancel_token.cancel();
        let _ = watcher_handle.join();

        assert!(
            reloaded,
            "a file in a directory created after the watcher started must still trigger a \
             reload via dynamic watch registration, got {} nodes",
            state.snapshot().contract_graph.node_count()
        );
    }
}
