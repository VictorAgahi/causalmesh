use crate::state::AppState;
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Reload routine invoked on the background pool after a relevant filesystem change.
/// Kept as a callback because the concrete indexer lives in `mesh-parsers`, which
/// `mesh-core` cannot depend on.
pub type ReloadFn = Arc<dyn Fn(&AppState) + Send + Sync>;

/// In-kernel filesystem watcher service providing debounced change notifications
/// and atomic snapshot reloading per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(150);

    /// Spawns the debounced file watcher actor in a background thread
    pub fn spawn(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        reload: ReloadFn,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Self::DEBOUNCE_INTERVAL, tx)?;

        for root in state.allowed_roots.iter() {
            if root.exists() {
                debouncer.watcher().watch(root, RecursiveMode::Recursive)?;
                tracing::info!(target: "mesh::watcher", "FileWatcher watching root: {}", root.display());

                // Watch .git/HEAD for branch checkouts / rebases
                let git_head = root.join(".git").join("HEAD");
                if git_head.exists() {
                    let _ = debouncer
                        .watcher()
                        .watch(&git_head, RecursiveMode::NonRecursive);
                }
            }
        }

        let handle = std::thread::Builder::new()
            .name("mesh-file-watcher".to_string())
            .spawn(move || {
                let _watcher = debouncer;

                loop {
                    if cancel_token.is_cancelled() {
                        tracing::info!(target: "mesh::watcher", "FileWatcher received cancellation signal, stopping.");
                        break;
                    }

                    match rx.recv_timeout(Duration::from_millis(300)) {
                        Ok(Ok(events)) => {
                            let relevant = events.iter().any(|ev| Self::is_relevant_path(&ev.path));
                            if relevant {
                                tracing::info!(
                                    target: "mesh::watcher",
                                    "Detected filesystem mutations ({} events). Scheduling atomic graph rescan.",
                                    events.len()
                                );
                                Self::schedule_reload(state.clone(), reload.clone());
                            }
                        }
                        Ok(Err(errs)) => {
                            tracing::warn!(target: "mesh::watcher", "File watch error: {:?}", errs);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;

        Ok(handle)
    }

    /// Queues one reload on the QoS-throttled pool. Bursts arriving before any
    /// worker has started are coalesced into it (a cheap, best-effort filter, not
    /// how correctness is guaranteed); a request arriving *during* a run spawns a
    /// second closure whose `reload` call (any implementation backed by
    /// `WorkspaceIndexer::reload`) blocks on `AppState::reload_lock` until the
    /// first pass finishes, then runs its own pass against then-current disk
    /// state. See that lock's own doc for why at most one reload ever runs at a
    /// time (idempotence invariant I2).
    pub fn schedule_reload(state: Arc<AppState>, reload: ReloadFn) {
        if state.reload_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let job_state = state.clone();
        drop(state.rescan.spawn(move || {
            job_state.reload_pending.store(false, Ordering::Release);
            reload(&job_state);
        }));
    }

    /// Determines if a changed path is relevant for index rebuilding
    pub fn is_relevant_path(path: &Path) -> bool {
        let mut components = path.components().map(|c| c.as_os_str());
        if components.any(|c| c == "target" || c == "node_modules") {
            return false;
        }

        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with('~') || name.ends_with(".swp") {
            return false;
        }

        // .git/HEAD and refs matter (checkout / rebase); objects and everything else in .git do not.
        let mut in_git = false;
        for c in path.components() {
            let c = c.as_os_str();
            if in_git {
                return c == "HEAD" || c == "refs";
            }
            in_git = c == ".git";
        }

        matches!(
            path.extension().and_then(|e| e.to_str()),
            Some(
                "rs" | "go"
                    | "java"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "py"
                    | "kt"
                    | "kts"
                    | "cs"
                    | "rb"
                    | "rake"
                    | "php"
                    | "phtml"
                    | "swift"
                    | "scala"
                    | "sc"
                    | "proto"
                    | "yaml"
                    | "yml"
                    | "properties"
                    | "md"
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
