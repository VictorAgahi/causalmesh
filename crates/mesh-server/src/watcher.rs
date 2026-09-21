use mesh_core::AppState;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// High-level FileWatcherService binding PolyglotIndexer to the in-kernel core watcher per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: std::time::Duration =
        mesh_core::FileWatcherService::DEBOUNCE_INTERVAL;

    /// Spawns the debounced file watcher actor wired with the polyglot indexer
    pub fn spawn(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        let patterns = state
            .config
            .load()
            .engines
            .contracts
            .as_ref()
            .map(|c| c.patterns.clone())
            .unwrap_or_default();
        let roots = state.allowed_roots.load_full();

        mesh_core::FileWatcherService::spawn(state, cancel_token, move |file, content, graph| {
            let repo_id = Self::repo_id_for_file(file, &roots);
            mesh_parsers::PolyglotIndexer::index_file(file, content, repo_id, graph);
            mesh_parsers::PolyglotIndexer::apply_custom_patterns(
                file, content, repo_id, &patterns, graph,
            );
        })
    }

    /// Finds which configured root a changed file belongs to, so hot-reloaded
    /// nodes keep the same `repo_id` (and therefore repo identity) they'd get
    /// from the initial full workspace scan.
    fn repo_id_for_file(file: &std::path::Path, roots: &[std::path::PathBuf]) -> mesh_core::RepoId {
        roots
            .iter()
            .position(|root| file.starts_with(root))
            .unwrap_or(0) as mesh_core::RepoId
    }

    #[inline]
    pub fn is_relevant_path(path: &std::path::Path) -> bool {
        mesh_core::FileWatcherService::is_relevant_path(path)
    }

    pub fn execute_reload_sync(state: &AppState) {
        let patterns = state
            .config
            .load()
            .engines
            .contracts
            .as_ref()
            .map(|c| c.patterns.clone())
            .unwrap_or_default();
        let roots = state.allowed_roots.load_full();

        mesh_core::FileWatcherService::execute_reload_sync(state, &|file, content, graph| {
            let repo_id = Self::repo_id_for_file(file, &roots);
            mesh_parsers::PolyglotIndexer::index_file(file, content, repo_id, graph);
            mesh_parsers::PolyglotIndexer::apply_custom_patterns(
                file, content, repo_id, &patterns, graph,
            );
        });
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
        assert_eq!(state.contract_graph.load().node_count(), 2);

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
            if state.contract_graph.load().node_count() > 2 {
                reloaded = true;
                break;
            }
        }

        cancel_token.cancel();
        let _ = watcher_handle.join();

        assert!(
            reloaded,
            "ContractGraph in AppState must be automatically reloaded by FileWatcherService upon file change! Expected > 2 nodes, got: {}",
            state.contract_graph.load().node_count()
        );
        assert_eq!(state.contract_graph.load().node_count(), 4);
    }
}
