//! The workspace `meshd` serves, and how it follows its config file.
//!
//! `AppState` is immutable in its `config` and `allowed_roots` (only the
//! snapshot hot-swaps), so a changed config means a new `AppState`. The
//! [`WorkspaceHost`] holds the current one behind an `ArcSwap`: on a request,
//! at most every [`CHECK_INTERVAL`], it reads and hashes the config file (a few
//! hundred bytes; mtime and length are not trusted, a same-length edit within
//! one mtime tick would hide behind them), and only a different hash rebuilds. The
//! rebuild (parse, expand roots, index, start a watcher) runs on a blocking
//! thread while the old state keeps serving, and the swap happens once the new
//! state has its first snapshot. A config that does not parse, or whose roots do
//! not resolve, is refused: the old state keeps serving, the reason is logged to
//! stderr and recorded for `mesh-mcp doctor`.

use arc_swap::ArcSwap;
use mesh_core::config_watch::{config_sha256, write_daemon_config_status_in, DaemonConfigStatus};
use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, PersistentIndexCache,
};
use mesh_server::{FileWatcherService, WorkspaceIndexer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Minimum time between two `stat`s of the config file.
pub const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// What one [`WorkspaceHost::reload_if_changed`] pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// Nothing changed (or another reload is already running).
    Unchanged,
    /// A new workspace state was built and is now serving.
    Reloaded,
    /// The new config was refused; the previous state keeps serving.
    Rejected(String),
}

struct Book {
    /// SHA-256 of the config bytes the serving state was built from.
    serving_sha: Option<String>,
    /// SHA-256 of the on-disk bytes last refused (so they are not retried until
    /// the file changes again); `None` when nothing is refused.
    rejected_sha: Option<String>,
    /// Cancels the file watcher of the serving state.
    watcher_cancel: CancellationToken,
    last_error: Option<String>,
}

pub struct WorkspaceHost {
    current: ArcSwap<AppState>,
    config_path: Option<PathBuf>,
    workspace_id: String,
    audit: Arc<AuditLogger>,
    rescan: Arc<BackgroundRescanEngine>,
    /// Parent of every watcher token: cancelled at daemon shutdown.
    shutdown: CancellationToken,
    started_ms: u128,
    epoch: Instant,
    last_check_ms: AtomicU64,
    reloading: AtomicBool,
    /// Held for one whole rebuild, so two never run at once.
    gate: Mutex<()>,
    book: Mutex<Book>,
    /// Where the status record and the index caches of rebuilt states go;
    /// `None` is the user's real cache directory. Tests point it at a tempdir.
    cache_dir: Option<PathBuf>,
    /// Test hook: make the next reload panic.
    #[cfg(test)]
    panic_next: AtomicBool,
}

/// Clears `reloading` when dropped, so even a reload that unwinds (or a task
/// that never runs) cannot leave the host unable to follow its config again.
struct ReloadingFlag(Arc<WorkspaceHost>);

impl Drop for ReloadingFlag {
    fn drop(&mut self) {
        self.0.reloading.store(false, Ordering::Release);
    }
}

/// Gives `state` a persistent index cache under `cache_dir` (unique per `tag`)
/// instead of the user's real per-workspace cache.
pub fn seed_index_cache(state: &AppState, cache_dir: &Path, tag: u64) {
    let quota = PersistentIndexCache::quota_bytes_from_mb(state.config.cache.max_size_mb);
    match PersistentIndexCache::open(cache_dir.join("index").join(format!("{tag}.db")), quota) {
        Ok(cache) => {
            let _ = state.index_cache.set(cache);
        }
        Err(e) => tracing::warn!(target: "meshd", "cannot open the seeded index cache: {e}"),
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Indexes `state`'s workspace into its first snapshot, under `reload_lock` like
/// every later reload (idempotence invariant I2). Blocking.
pub fn ingest_blocking(state: &Arc<AppState>, base_dir: &Path) {
    let _guard = state.reload_lock.lock().unwrap_or_else(|p| p.into_inner());
    tracing::info!(target: "meshd", "Starting initial workspace ingestion...");
    // Per-workspace cache under its quota (plan 4 step 4.4); a failure to open it
    // degrades to "parse everything" (see `AppState::open_index_cache`).
    let index_cache = state.open_index_cache(base_dir);
    let snapshot = {
        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());
        WorkspaceIndexer::build_snapshot(
            &state.config,
            &state.allowed_roots,
            None,
            Some(&mut vfs),
            index_cache,
        )
    };
    state.install_snapshot(snapshot);
    tracing::info!(
        target: "meshd",
        "Ingestion complete: {} contract nodes indexed.",
        state.snapshot().contract_graph.node_count()
    );
}

/// Starts the shared file watcher of `state`, then closes the gap between
/// "watches are live" and "the snapshot was stale by then" with one
/// differential reload (see `meshd`'s main for the full reasoning). Blocking.
pub fn start_watcher_blocking(state: &Arc<AppState>, cancel: CancellationToken) {
    match FileWatcherService::spawn(state.clone(), cancel) {
        Ok(_handle) => FileWatcherService::execute_reload_sync(state),
        Err(e) => tracing::warn!(target: "meshd", "Failed to start FileWatcherService: {e}"),
    }
}

impl WorkspaceHost {
    /// A host serving `state`, which was built from `config_path` (`None` for the
    /// built-in default config: nothing to follow). `serving_sha` is the SHA-256
    /// of the very bytes `state`'s `Config` was parsed from (from the same read:
    /// `WorkspaceIndexer::discover_config_stamped`), never a later re-read of the
    /// file, which could describe newer content than the state serves.
    /// `watcher_cancel` stops the watcher already running on `state`; it must be
    /// a child of `shutdown`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: Arc<AppState>,
        config_path: Option<PathBuf>,
        serving_sha: Option<String>,
        workspace_id: &str,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
        shutdown: CancellationToken,
        watcher_cancel: CancellationToken,
    ) -> Arc<Self> {
        Self::new_in(
            state,
            config_path,
            serving_sha,
            workspace_id,
            audit,
            rescan,
            shutdown,
            watcher_cancel,
            None,
        )
    }

    /// [`Self::new`] with the cache directory given (`None`: the real one).
    #[allow(clippy::too_many_arguments)]
    pub fn new_in(
        state: Arc<AppState>,
        config_path: Option<PathBuf>,
        serving_sha: Option<String>,
        workspace_id: &str,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
        shutdown: CancellationToken,
        watcher_cancel: CancellationToken,
        cache_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let host = Arc::new(Self {
            current: ArcSwap::from(state),
            config_path,
            workspace_id: workspace_id.to_string(),
            audit,
            rescan,
            shutdown,
            started_ms: now_ms(),
            epoch: Instant::now(),
            last_check_ms: AtomicU64::new(0),
            reloading: AtomicBool::new(false),
            gate: Mutex::new(()),
            book: Mutex::new(Book {
                serving_sha,
                rejected_sha: None,
                watcher_cancel,
                last_error: None,
            }),
            cache_dir,
            #[cfg(test)]
            panic_next: AtomicBool::new(false),
        });
        // Not published here: a daemon that then loses the bind race for its
        // socket must not overwrite the winner's record (see `write_daemon_meta`'s
        // caller). The server publishes once it is bound.
        host
    }

    /// A host that never reloads: tests and embedders with a fixed state.
    pub fn fixed(state: Arc<AppState>) -> Arc<Self> {
        let audit = Arc::clone(&state.audit);
        let rescan = Arc::clone(&state.rescan);
        let shutdown = CancellationToken::new();
        let watcher = shutdown.child_token();
        Arc::new(Self {
            current: ArcSwap::from(state),
            config_path: None,
            workspace_id: String::new(),
            audit,
            rescan,
            shutdown,
            started_ms: now_ms(),
            epoch: Instant::now(),
            last_check_ms: AtomicU64::new(0),
            reloading: AtomicBool::new(false),
            gate: Mutex::new(()),
            book: Mutex::new(Book {
                serving_sha: None,
                rejected_sha: None,
                watcher_cancel: watcher,
                last_error: None,
            }),
            cache_dir: None,
            #[cfg(test)]
            panic_next: AtomicBool::new(false),
        })
    }

    /// The state to answer a request with. Also notices a changed config and, if
    /// it changed, starts the rebuild in the background: this request and the ones
    /// until the rebuild is ready are answered by the state already serving.
    pub fn current(self: &Arc<Self>) -> Arc<AppState> {
        self.check_for_change();
        self.current.load_full()
    }

    /// The serving state, without checking the config file.
    #[cfg(test)]
    pub fn peek(&self) -> Arc<AppState> {
        self.current.load_full()
    }

    fn check_for_change(self: &Arc<Self>) {
        let Some(path) = self.config_path.as_deref() else {
            return;
        };
        let elapsed = self.epoch.elapsed().as_millis() as u64;
        let last = self.last_check_ms.load(Ordering::Relaxed);
        if last != 0 && elapsed.saturating_sub(last) < CHECK_INTERVAL.as_millis() as u64 {
            return;
        }
        // `max(1)`: 0 means "never checked".
        self.last_check_ms.store(elapsed.max(1), Ordering::Relaxed);
        // Read and hash every time the throttle window passes: the file is tiny
        // and `(mtime, len)` cannot see a same-length edit within one mtime tick.
        let moved = match std::fs::read(path) {
            Ok(bytes) => self.observe(&config_sha256(&bytes)),
            Err(_) => false,
        };
        if !moved {
            return;
        }
        if self.reloading.swap(true, Ordering::AcqRel) {
            return;
        }
        // Released by `Drop`, whatever happens to the reload (or the task).
        let flag = ReloadingFlag(Arc::clone(self));
        let host = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _flag = flag;
            host.reload_guarded();
        });
    }

    /// Whether the on-disk content `sha` calls for a reload. Also clears a
    /// recorded rejection once the file is back to what is being served.
    fn observe(&self, sha: &str) -> bool {
        let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
        if book.serving_sha.as_deref() == Some(sha) {
            let healed = book.last_error.take().is_some();
            book.rejected_sha = None;
            drop(book);
            if healed {
                self.publish_status();
            }
            return false;
        }
        book.rejected_sha.as_deref() != Some(sha)
    }

    /// [`Self::reload_if_changed`] that turns a panic into a recorded refusal:
    /// the old state keeps serving, the panic is logged to stderr and
    /// `doctor` sees it as the last error, and the same bytes are not retried.
    pub fn reload_guarded(&self) -> ReloadOutcome {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reload_if_changed())) {
            Ok(outcome) => outcome,
            Err(payload) => {
                let what = payload
                    .downcast_ref::<&str>()
                    .map(|m| (*m).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let shown = self
                    .config_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let sha = self
                    .config_path
                    .as_deref()
                    .and_then(|p| std::fs::read(p).ok())
                    .map(|b| config_sha256(&b));
                self.reject(
                    sha,
                    format!(
                        "reloading config {shown} panicked: {what}; still serving the previous config"
                    ),
                )
            }
        }
    }

    /// Records which config this daemon serves, for `mesh-mcp doctor`.
    pub fn publish_status(&self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let book = self.book.lock().unwrap_or_else(|p| p.into_inner());
        let dir = self
            .cache_dir
            .clone()
            .unwrap_or_else(mesh_core::paths::mesh_cache_dir);
        write_daemon_config_status_in(
            &dir,
            &self.workspace_id,
            &DaemonConfigStatus {
                started_ms: self.started_ms,
                config_path: Some(path),
                config_sha256: book.serving_sha.clone(),
                last_error: book.last_error.clone(),
                rejected_sha256: book.rejected_sha.clone(),
            },
        );
    }

    fn reject(&self, rejected_sha: Option<String>, message: String) -> ReloadOutcome {
        tracing::warn!(target: "meshd::config", "{message}");
        {
            let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            book.rejected_sha = rejected_sha;
            book.last_error = Some(message.clone());
        }
        self.publish_status();
        ReloadOutcome::Rejected(message)
    }

    /// Compares the config file with the one the serving state was built from and
    /// rebuilds when its content differs. Blocking (parses and indexes the
    /// workspace); the old state serves until the new one has its first snapshot.
    #[cfg_attr(test, allow(clippy::panic))] // the injected panic of the tests
    pub fn reload_if_changed(&self) -> ReloadOutcome {
        let Some(path) = self.config_path.as_deref() else {
            return ReloadOutcome::Unchanged;
        };
        let _gate = match self.gate.try_lock() {
            Ok(g) => g,
            // A reload that panicked poisoned the gate: that must not stop the
            // next one.
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return ReloadOutcome::Unchanged,
        };
        // One read: these bytes are hashed, parsed and, if they win, recorded as
        // the served content, so the recorded hash always describes the state.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            // Vanished (an editor's atomic save, or deleted): keep serving.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReloadOutcome::Unchanged,
            Err(e) => {
                return self.reject(
                    None,
                    format!(
                        "cannot read config {}: {e}; still serving the previous config",
                        path.display()
                    ),
                )
            }
        };
        let sha = config_sha256(&bytes);
        if !self.observe(&sha) {
            return ReloadOutcome::Unchanged;
        }
        #[cfg(test)]
        {
            if self.panic_next.swap(false, Ordering::SeqCst) {
                panic!("injected reload panic");
            }
        }
        let shown = path.display();
        let (config, base_dir) = match WorkspaceIndexer::parse_config_bytes(path, &bytes) {
            Ok(loaded) => loaded,
            Err(e) => {
                return self.reject(
                    Some(sha),
                    format!(
                    "config {shown} changed but is invalid: {e}; still serving the previous config"
                ),
                )
            }
        };
        let roots = match expand_roots(
            &config.workspace.roots,
            &base_dir,
            &config.workspace.workspace_root,
        ) {
            Ok(r) => r,
            Err(e) => {
                return self.reject(
                    Some(sha),
                    format!("config {shown} changed but its roots do not resolve: {e}; still serving the previous config"),
                )
            }
        };

        tracing::info!(
            target: "meshd::config",
            "Config {shown} changed; rebuilding the workspace state ({} root(s)) while the previous one keeps serving.",
            roots.len()
        );
        let state = Arc::new(AppState::new(
            config,
            roots,
            Arc::clone(&self.audit),
            Arc::clone(&self.rescan),
        ));
        if let Some(dir) = &self.cache_dir {
            static TAG: AtomicU64 = AtomicU64::new(1);
            seed_index_cache(&state, dir, TAG.fetch_add(1, Ordering::Relaxed));
        }
        ingest_blocking(&state, &base_dir);
        let watcher_cancel = self.shutdown.child_token();
        start_watcher_blocking(&state, watcher_cancel.clone());

        self.current.store(Arc::clone(&state));
        let old_watcher = {
            let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            book.serving_sha = Some(sha);
            book.rejected_sha = None;
            book.last_error = None;
            std::mem::replace(&mut book.watcher_cancel, watcher_cancel)
        };
        old_watcher.cancel();
        self.publish_status();
        tracing::info!(target: "meshd::config", "Now serving the new config ({shown}).");
        ReloadOutcome::Reloaded
    }
}

/// What the IPC server hands each connection: a [`WorkspaceHost`], or a fixed
/// `AppState` (tests) wrapped in one that never reloads.
#[derive(Clone)]
pub struct StateHandle(pub Arc<WorkspaceHost>);

impl StateHandle {
    pub fn current(&self) -> Arc<AppState> {
        self.0.current()
    }

    pub fn publish_status(&self) {
        self.0.publish_status();
    }
}

impl From<Arc<AppState>> for StateHandle {
    fn from(state: Arc<AppState>) -> Self {
        Self(WorkspaceHost::fixed(state))
    }
}

impl From<Arc<WorkspaceHost>> for StateHandle {
    fn from(host: Arc<WorkspaceHost>) -> Self {
        Self(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{AuditLogger, BackgroundRescanEngine};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        cache: PathBuf,
        config: PathBuf,
        host: Arc<WorkspaceHost>,
        shutdown: CancellationToken,
    }

    fn write_config(path: &Path, roots: &[&str]) {
        let list: Vec<String> = roots.iter().map(|r| format!("\"{r}\"")).collect();
        std::fs::write(
            path,
            format!("[workspace]\nname = \"w\"\nroots = [{}]\n", list.join(", ")),
        )
        .expect("write config");
    }

    fn fixture(roots: &[&str]) -> Fixture {
        fixture_with(roots, |_| {})
    }

    /// `after_load` runs once the state's config has been read and before the
    /// host is created: an edit there is one the state was not built from.
    fn fixture_with(roots: &[&str], after_load: impl FnOnce(&Path)) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        for sub in ["a", "b"] {
            std::fs::create_dir_all(root.join(sub)).expect("mkdir");
            std::fs::write(
                root.join(sub).join("lib.ts"),
                format!("export class Only{} {{}}\n", sub.to_uppercase()),
            )
            .expect("write");
        }
        let config = root.join("mesh-mcp.toml");
        write_config(&config, roots);
        let (cfg, base, digest) = WorkspaceIndexer::load_config_stamped(&config).expect("load");
        after_load(&config);
        let resolved = expand_roots(&cfg.workspace.roots, &base, &cfg.workspace.workspace_root)
            .expect("roots");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, resolved, audit.clone(), rescan.clone()));
        // Never the developer's real cache: index db and status record both live
        // in this test's own tempdir.
        let cache = root.join(".test-cache");
        seed_index_cache(&state, &cache, 0);
        ingest_blocking(&state, &base);
        let shutdown = CancellationToken::new();
        let host = WorkspaceHost::new_in(
            state,
            Some(config.clone()),
            Some(digest),
            "test-ws-host",
            audit,
            rescan,
            shutdown.clone(),
            shutdown.child_token(),
            Some(cache.clone()),
        );
        Fixture {
            _dir: dir,
            root,
            cache,
            config,
            host,
            shutdown,
        }
    }

    fn knows(state: &AppState, class: &str) -> bool {
        state
            .snapshot()
            .contract_graph
            .all_nodes()
            .any(|n| n.name.as_str() == class)
    }

    #[test]
    fn an_edited_config_is_picked_up_without_a_restart() {
        let fx = fixture(&["a"]);
        let before = fx.host.peek();
        assert!(knows(&before, "OnlyA"));
        assert!(!knows(&before, "OnlyB"));
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);

        write_config(&fx.config, &["a", "b"]);
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);

        let after = fx.host.peek();
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(knows(&after, "OnlyA") && knows(&after, "OnlyB"));
        assert_eq!(after.allowed_roots.len(), 2);
        // The previous state is untouched for requests still holding it.
        assert!(!knows(&before, "OnlyB"));
        // Unchanged content afterwards: no further rebuild.
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);
        fx.shutdown.cancel();
    }

    #[test]
    fn a_touch_without_a_content_change_does_not_rebuild() {
        let fx = fixture(&["a"]);
        let before = fx.host.peek();
        let text = std::fs::read_to_string(&fx.config).expect("read");
        std::fs::write(&fx.config, text).expect("rewrite identical");
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);
        assert!(Arc::ptr_eq(&before, &fx.host.peek()));
        fx.shutdown.cancel();
    }

    #[test]
    fn an_invalid_config_keeps_the_old_state_and_records_why() {
        let fx = fixture(&["a"]);
        let before = fx.host.peek();

        std::fs::write(&fx.config, "[workspace\nthis is not toml").expect("write garbage");
        let outcome = fx.host.reload_if_changed();
        assert!(
            matches!(outcome, ReloadOutcome::Rejected(_)),
            "expected a rejection, got {outcome:?}"
        );
        let message = match outcome {
            ReloadOutcome::Rejected(m) => m,
            _ => String::new(),
        };
        assert!(message.contains("invalid"), "{message}");
        assert!(
            message.contains("still serving the previous config"),
            "{message}"
        );
        assert!(Arc::ptr_eq(&before, &fx.host.peek()));
        assert!(knows(&fx.host.peek(), "OnlyA"));

        // Not retried while the file stays as it is.
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);

        // Roots that resolve to nothing are refused the same way.
        write_config(&fx.config, &["does-not-exist"]);
        assert!(matches!(
            fx.host.reload_if_changed(),
            ReloadOutcome::Rejected(_)
        ));
        assert!(Arc::ptr_eq(&before, &fx.host.peek()));

        // A fixed config recovers on the next pass.
        write_config(&fx.config, &["b"]);
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);
        assert!(knows(&fx.host.peek(), "OnlyB"));
        assert!(!knows(&fx.host.peek(), "OnlyA"));
        assert!(fx.root.exists());
        fx.shutdown.cancel();
    }

    #[test]
    fn a_vanished_config_keeps_serving() {
        let fx = fixture(&["a"]);
        let before = fx.host.peek();
        std::fs::remove_file(&fx.config).expect("remove");
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);
        assert!(Arc::ptr_eq(&before, &fx.host.peek()));
        fx.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_request_notices_the_change_and_the_old_state_serves_until_ready() {
        let fx = fixture(&["a"]);
        let before = fx.host.current();
        write_config(&fx.config, &["a", "b"]);
        // The first request after the edit starts the rebuild and is still
        // answered by the previous state.
        let during = fx.host.current();
        assert!(Arc::ptr_eq(&before, &during) || knows(&during, "OnlyB"));
        let mut served_new = false;
        for _ in 0..200 {
            if knows(&fx.host.current(), "OnlyB") {
                served_new = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(served_new, "the rebuilt state never started serving");
        fx.shutdown.cancel();
    }

    fn set_mtime(path: &Path, secs: u64) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .expect("set mtime");
    }

    #[test]
    fn a_same_length_edit_with_an_identical_mtime_is_still_reloaded() {
        let fx = fixture(&["a"]);
        set_mtime(&fx.config, 1_700_000_000);
        // Re-baseline on the pinned mtime (content is unchanged: no rebuild).
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Unchanged);
        let before_len = std::fs::metadata(&fx.config).expect("meta").len();
        // "a" -> "b": same length, and the mtime is put back to the same value.
        write_config(&fx.config, &["b"]);
        set_mtime(&fx.config, 1_700_000_000);
        assert_eq!(
            std::fs::metadata(&fx.config).expect("meta").len(),
            before_len
        );
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);
        assert!(knows(&fx.host.peek(), "OnlyB"));
        assert!(!knows(&fx.host.peek(), "OnlyA"));
        fx.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_request_notices_a_same_length_same_mtime_edit() {
        let fx = fixture(&["a"]);
        set_mtime(&fx.config, 1_700_000_000);
        write_config(&fx.config, &["b"]);
        set_mtime(&fx.config, 1_700_000_000);
        let _ = fx.host.current();
        let mut served_new = false;
        for _ in 0..200 {
            if knows(&fx.host.current(), "OnlyB") {
                served_new = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(served_new, "the same-length edit was never picked up");
        fx.shutdown.cancel();
    }

    #[test]
    fn the_served_hash_comes_from_the_bytes_the_state_was_built_from() {
        // The file is edited after the state's config was read but before the
        // host exists: the host must still see it as newer than what it serves.
        let fx = fixture_with(&["a"], |config| write_config(config, &["a", "b"]));
        assert!(!knows(&fx.host.peek(), "OnlyB"));
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);
        assert!(knows(&fx.host.peek(), "OnlyA") && knows(&fx.host.peek(), "OnlyB"));
        fx.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_panicking_reload_is_recorded_and_the_host_keeps_following_the_config() {
        let fx = fixture(&["a"]);
        let before = fx.host.current();
        fx.host.panic_next.store(true, Ordering::SeqCst);
        write_config(&fx.config, &["a", "b"]);
        // Past the throttle window of the `current()` above.
        tokio::time::sleep(CHECK_INTERVAL + Duration::from_millis(100)).await;
        let _ = fx.host.current();
        // Wait for the background task to finish (the flag is cleared by a guard).
        for _ in 0..200 {
            if !fx.host.reloading.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!fx.host.reloading.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(&before, &fx.host.peek()), "old state serves");
        let recorded = fx
            .host
            .book
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_error
            .clone()
            .unwrap_or_default();
        assert!(recorded.contains("panicked"), "{recorded}");
        assert!(recorded.contains("injected reload panic"), "{recorded}");
        // The status record `doctor` reads says so too, in the test's own cache.
        let status =
            mesh_core::config_watch::daemon_config_status_path_in(&fx.cache, "test-ws-host");
        assert!(std::fs::read_to_string(status)
            .expect("status written under the test cache")
            .contains("panicked"));

        // The same bytes are not retried, but a new edit is reloaded: the host
        // follows the config again.
        write_config(&fx.config, &["b"]);
        let mut served_new = false;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if knows(&fx.host.current(), "OnlyB") {
                served_new = true;
                break;
            }
        }
        assert!(
            served_new,
            "the host never followed the config after a panic"
        );
        fx.shutdown.cancel();
    }

    #[test]
    fn a_poisoned_gate_does_not_stop_later_reloads() {
        let fx = fixture(&["a"]);
        fx.host.panic_next.store(true, Ordering::SeqCst);
        write_config(&fx.config, &["a", "b"]);
        assert!(matches!(
            fx.host.reload_guarded(),
            ReloadOutcome::Rejected(_)
        ));
        write_config(&fx.config, &["b"]);
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);
        fx.shutdown.cancel();
    }

    #[test]
    fn nothing_is_written_outside_the_test_cache() {
        let fx = fixture(&["a"]);
        write_config(&fx.config, &["a", "b"]);
        assert_eq!(fx.host.reload_if_changed(), ReloadOutcome::Reloaded);
        let status =
            mesh_core::config_watch::daemon_config_status_path_in(&fx.cache, "test-ws-host");
        assert!(status.exists(), "{}", status.display());
        assert!(fx.cache.join("index").exists());
        fx.shutdown.cancel();
    }
}
