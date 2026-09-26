use crate::crawler::{ExcludeMatcher, FilesystemCrawler};
use crate::state::AppState;
use notify::{Config as NotifyConfig, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_mini::{
    new_debouncer, new_debouncer_opt, Config as DebouncerConfig, Debouncer,
};
use std::collections::HashSet;
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

/// Either watcher backend `spawn` can end up running: per-directory native watches (inotify/
/// FSEvents/ReadDirectoryChangesW) in the common case, or a polling backend when a workspace
/// has more watchable directories than `FileWatcherService::MAX_WATCHED_DIRS` — native watch
/// registration has a real per-process/per-user OS ceiling (Linux's `fs.inotify.max_user_watches`
/// most concretely), and a workspace that would exceed it must degrade to polling rather than
/// fail to start or silently stop watching partway through a giant tree.
enum AnyDebouncer {
    Native(Debouncer<RecommendedWatcher>),
    Poll(Debouncer<PollWatcher>),
}

impl AnyDebouncer {
    fn watcher(&mut self) -> &mut dyn Watcher {
        match self {
            AnyDebouncer::Native(d) => d.watcher(),
            AnyDebouncer::Poll(d) => d.watcher(),
        }
    }
}

/// In-kernel filesystem watcher service providing debounced change notifications
/// and atomic snapshot reloading per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(150);

    /// Above this many individually watchable directories across all roots, `spawn` gives up
    /// on per-directory native registration and falls back to `POLL_FALLBACK_INTERVAL` polling
    /// instead (P2 step 3.3). Deliberately conservative: some CI/container images and older
    /// Linux defaults leave `fs.inotify.max_user_watches` far below the 8192+ many desktop
    /// distros now ship, and macOS's FSEvents backend doesn't need per-directory registration
    /// to work correctly at all (see `plan_watch_dirs`'s use here), so this cap exists purely
    /// to protect the Linux case rather than being tuned to any single platform's true limit.
    pub const MAX_WATCHED_DIRS: usize = 4096;

    /// Poll interval used only in the capped fallback above. Coarser than
    /// `DEBOUNCE_INTERVAL` on purpose: polling a huge tree (the only workspaces that ever
    /// reach this fallback) every 150ms would itself be real, avoidable CPU/IO cost.
    pub const POLL_FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

    /// Walks `root` for directories to individually watch (gitignore/`exclude_patterns`-aware,
    /// `FilesystemCrawler::plan_watch_dirs`), consuming from `budget` (shared across every
    /// root in one `spawn` call, so the *combined* directory count across all roots is what's
    /// capped against `MAX_WATCHED_DIRS`, not each root independently).
    fn plan_root(
        root: &Path,
        matcher: &ExcludeMatcher,
        budget: usize,
    ) -> crate::crawler::WatchPlan {
        FilesystemCrawler::plan_watch_dirs(root, matcher, Some(10), budget)
    }

    /// Spawns the debounced file watcher actor in a background thread
    pub fn spawn(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        reload: ReloadFn,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        let (tx, rx) = std::sync::mpsc::channel();

        let matcher = ExcludeMatcher::compile(&state.config.workspace.exclude_patterns);
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
        let mut per_root_dirs: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
        let mut capped = false;
        for root in state.allowed_roots.iter() {
            if !root.exists() {
                continue;
            }
            let budget = Self::MAX_WATCHED_DIRS.saturating_sub(watched_dirs.len());
            let plan = Self::plan_root(root, &matcher, budget);
            if plan.capped {
                capped = true;
                break;
            }
            watched_dirs.extend(plan.dirs.iter().cloned());
            per_root_dirs.push((root.clone(), plan.dirs));
        }

        let mut debouncer = if capped {
            tracing::warn!(
                target: "mesh::watcher",
                "Workspace has more than {} watchable directories; falling back to polling \
                 every {:?} instead of native per-directory watches.",
                Self::MAX_WATCHED_DIRS,
                Self::POLL_FALLBACK_INTERVAL,
            );
            let config = DebouncerConfig::default()
                .with_timeout(Self::DEBOUNCE_INTERVAL)
                .with_notify_config(
                    NotifyConfig::default().with_poll_interval(Self::POLL_FALLBACK_INTERVAL),
                );
            AnyDebouncer::Poll(new_debouncer_opt::<_, PollWatcher>(config, tx)?)
        } else {
            AnyDebouncer::Native(new_debouncer(Self::DEBOUNCE_INTERVAL, tx)?)
        };

        if capped {
            // Polling backend: one recursive watch per root, same as this service's
            // behaviour before step 3.3. `PollWatcher` re-scans the whole tree on its own
            // interval rather than relying on per-directory kernel registration, so it
            // needs no gitignore-aware directory enumeration to sidestep an OS watch limit
            // that doesn't apply to it in the first place — that's the entire reason this
            // fallback exists.
            for root in state.allowed_roots.iter() {
                if root.exists() {
                    debouncer.watcher().watch(root, RecursiveMode::Recursive)?;
                    tracing::info!(target: "mesh::watcher", "FileWatcher (polling) watching root: {}", root.display());
                }
            }
        } else {
            for (root, dirs) in &per_root_dirs {
                for dir in dirs {
                    if let Err(e) = debouncer.watcher().watch(dir, RecursiveMode::NonRecursive) {
                        tracing::warn!(target: "mesh::watcher", "Failed to watch {}: {e}", dir.display());
                    }
                }
                tracing::info!(
                    target: "mesh::watcher",
                    "FileWatcher watching {} director{} under root: {}",
                    dirs.len(),
                    if dirs.len() == 1 { "y" } else { "ies" },
                    root.display()
                );

                // Watch .git/HEAD for branch checkouts / rebases
                let git_head = root.join(".git").join("HEAD");
                if git_head.exists() {
                    let _ = debouncer
                        .watcher()
                        .watch(&git_head, RecursiveMode::NonRecursive);
                }
            }
        }

        // `Arc<Mutex<_>>`, not owned outright by the loop thread: a native backend's `.watch()`
        // call is not the cheap syscall it is on Linux (inotify) everywhere — macOS's FSEvents
        // backend stops and restarts its whole event stream on every single `.watch()` call,
        // which this crate's own `stop()` implements as a busy-wait for the stream's background
        // runloop to become idle. Measured on this machine: a single dynamic registration took
        // over 11 seconds under load. Doing that inline on the thread that also has to keep
        // draining `rx` would stall every *other* pending reload for however long that takes.
        // The `Mutex` lets `Self::register_new_directories` dispatch the actual `.watch()` calls
        // onto `state.rescan`'s background pool instead, so the event-receive loop below is
        // never blocked by them (see there for the synchronous/deferred split).
        let debouncer = Arc::new(std::sync::Mutex::new(debouncer));

        let handle = std::thread::Builder::new()
            .name("mesh-file-watcher".to_string())
            .spawn(move || {
                let debouncer_keepalive = debouncer;
                let mut watched_dirs = watched_dirs;
                let roots = state.allowed_roots.clone();

                loop {
                    if cancel_token.is_cancelled() {
                        tracing::info!(target: "mesh::watcher", "FileWatcher received cancellation signal, stopping.");
                        break;
                    }

                    match rx.recv_timeout(Duration::from_millis(300)) {
                        Ok(Ok(events)) => {
                            // Native (non-polling) mode only: a directory this run didn't know
                            // about at startup (created after `spawn`, e.g. `mkdir`, a branch
                            // checkout, an archive extraction) has no watch registered on it yet
                            // — unlike the old single-recursive-watch design, per-directory
                            // registration doesn't track new subdirectories automatically. Any
                            // event path that is now a directory, isn't already watched, and
                            // isn't itself excluded gets a fresh watch; `plan_root` also picks
                            // up any of *its* qualifying subdirectories in case a whole subtree
                            // (not just one empty directory) appeared in a single burst. The
                            // polling backend needs none of this — it re-scans everything on its
                            // own interval regardless of what's registered.
                            if !capped {
                                for ev in &events {
                                    if watched_dirs.len() >= Self::MAX_WATCHED_DIRS {
                                        tracing::warn!(
                                            target: "mesh::watcher",
                                            "Reached {} watched directories; new subdirectories \
                                             under {} will not be individually watched until the \
                                             next restart.",
                                            Self::MAX_WATCHED_DIRS,
                                            ev.path.display()
                                        );
                                        break;
                                    }
                                    if watched_dirs.contains(&ev.path) || !ev.path.is_dir() {
                                        continue;
                                    }
                                    let Some(root) =
                                        roots.iter().find(|r| ev.path.starts_with(r.as_path()))
                                    else {
                                        continue;
                                    };
                                    let budget = Self::MAX_WATCHED_DIRS - watched_dirs.len();
                                    // Enumeration (a filesystem walk, no OS watch API involved)
                                    // is cheap — measured under 1ms here — and stays synchronous
                                    // so `watched_dirs` bookkeeping (below) doesn't race a second
                                    // event for the same new directory arriving before the
                                    // deferred `.watch()` calls below have run.
                                    let plan = Self::plan_root(&ev.path, &matcher, budget);
                                    if plan.capped {
                                        continue;
                                    }
                                    let new_dirs: Vec<PathBuf> = plan
                                        .dirs
                                        .into_iter()
                                        .filter(|d| watched_dirs.insert(d.clone()))
                                        .collect();
                                    if new_dirs.is_empty() {
                                        continue;
                                    }
                                    tracing::debug!(
                                        target: "mesh::watcher",
                                        "Registering {} new director{} under {} (root {}) in the background.",
                                        new_dirs.len(),
                                        if new_dirs.len() == 1 { "y" } else { "ies" },
                                        ev.path.display(),
                                        root.display()
                                    );
                                    let debouncer_for_task = debouncer_keepalive.clone();
                                    state.rescan.spawn(move || {
                                        let mut guard = debouncer_for_task
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        for dir in &new_dirs {
                                            if let Err(e) =
                                                guard.watcher().watch(dir, RecursiveMode::NonRecursive)
                                            {
                                                tracing::warn!(target: "mesh::watcher", "Failed to watch new directory {}: {e}", dir.display());
                                            }
                                        }
                                    });
                                }
                            }

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
