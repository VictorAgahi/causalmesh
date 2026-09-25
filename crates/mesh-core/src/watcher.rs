use crate::state::AppState;
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Reload routine invoked on the background pool after a relevant filesystem change,
/// given the specific paths the watcher observed change (coalesced across every
/// burst folded into this one run — see `schedule_reload`) so it can index those
/// paths directly instead of re-crawling the whole tree to rediscover them. Kept
/// as a callback because the concrete indexer lives in `mesh-parsers`, which
/// `mesh-core` cannot depend on.
pub type ReloadFn = Arc<dyn Fn(&AppState, &[PathBuf]) + Send + Sync>;

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
                            let relevant_paths: Vec<PathBuf> = events
                                .iter()
                                .map(|ev| ev.path.clone())
                                .filter(|p| Self::is_relevant_path(p))
                                .collect();
                            if !relevant_paths.is_empty() {
                                tracing::info!(
                                    target: "mesh::watcher",
                                    "Detected filesystem mutations ({} relevant of {} events). Scheduling atomic graph rescan.",
                                    relevant_paths.len(),
                                    events.len()
                                );
                                Self::schedule_reload(state.clone(), reload.clone(), &relevant_paths);
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

    /// Queues one reload on the QoS-throttled pool, carrying `paths` for a
    /// targeted, path-driven reload (`WorkspaceIndexer::reload_paths`) instead of
    /// a full crawl. `paths` is appended to `AppState::pending_reload_paths`
    /// *before* the coalescing check below, so a burst that arrives while a job
    /// is already queued or running never has its paths silently dropped — it
    /// still returns early (the queued/running job's `Rayon` slot is not
    /// duplicated), but the paths it carried are picked up by whichever job
    /// drains the accumulator next, this one or a subsequent one. That drain is
    /// racy against a new job being spawned right as one finishes (see the
    /// interleavings below), but each amounts to at most one redundant harmless
    /// empty-`paths` reload — no path is ever silently lost, which is the actual
    /// correctness requirement `reload_pending`'s "best-effort, not exclusive"
    /// coalescing has always carried (see `reload_lock`'s own doc for why at
    /// most one reload ever *installs a snapshot* at a time, idempotence
    /// invariant I2 — unaffected by this).
    pub fn schedule_reload(state: Arc<AppState>, reload: ReloadFn, paths: &[PathBuf]) {
        {
            let mut pending = state
                .pending_reload_paths
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.extend(paths.iter().cloned());
        }
        if state.reload_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let job_state = state.clone();
        drop(state.rescan.spawn(move || {
            job_state.reload_pending.store(false, Ordering::Release);
            let drained: Vec<PathBuf> = {
                let mut pending = job_state
                    .pending_reload_paths
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                std::mem::take(&mut *pending)
            };
            reload(&job_state, &drained);
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

    fn test_state() -> Arc<AppState> {
        let cfg = crate::Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n",
        )
        .expect("config");
        let audit = Arc::new(crate::AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(crate::BackgroundRescanEngine::new().expect("rescan"));
        Arc::new(AppState::new(cfg, vec![PathBuf::from(".")], audit, rescan))
    }

    /// `schedule_reload` must hand the exact paths it was given to the `ReloadFn`
    /// it queues — the whole point of threading paths through at all is a
    /// targeted reload that doesn't need to re-crawl to know what changed.
    #[test]
    fn schedule_reload_delivers_the_given_paths_to_the_reload_fn() {
        let state = test_state();
        let received: Arc<std::sync::Mutex<Option<Vec<PathBuf>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let received_clone = received.clone();
        let reload: ReloadFn = Arc::new(move |_state, paths| {
            *received_clone.lock().unwrap() = Some(paths.to_vec());
        });

        let paths = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        FileWatcherService::schedule_reload(state, reload, &paths);

        let mut got = None;
        for _ in 0..50 {
            if let Some(v) = received.lock().unwrap().clone() {
                got = Some(v);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(got, Some(paths));
    }

    /// A second burst arriving while a job is already queued/running must not
    /// have its paths silently dropped — they must still reach some reload call,
    /// even though `reload_pending` coalesces the two calls into fewer job spawns.
    /// This exercises the accumulator directly (bypassing the timing-dependent
    /// spawn race) by pushing to `pending_reload_paths` the same way
    /// `schedule_reload` does, then draining it the same way the queued job does.
    #[test]
    fn pending_reload_paths_accumulates_across_coalesced_bursts() {
        let state = test_state();
        {
            let mut pending = state.pending_reload_paths.lock().unwrap();
            pending.extend([PathBuf::from("a.rs")]);
        }
        {
            let mut pending = state.pending_reload_paths.lock().unwrap();
            pending.extend([PathBuf::from("b.rs")]);
        }
        let drained: Vec<PathBuf> = {
            let mut pending = state.pending_reload_paths.lock().unwrap();
            std::mem::take(&mut *pending)
        };
        assert_eq!(drained, vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
        assert!(state.pending_reload_paths.lock().unwrap().is_empty());
    }
}
