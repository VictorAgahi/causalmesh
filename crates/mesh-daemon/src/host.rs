//! The workspace `meshd` serves, and how it follows its config file.
//!
//! `AppState` is immutable in its `config` and `allowed_roots` (only the
//! snapshot hot-swaps), so a changed config means a new `AppState`. The
//! [`WorkspaceHost`] holds the current one behind an `ArcSwap`: on a request,
//! at most every [`CHECK_INTERVAL`], it `stat`s the config file; when mtime or
//! length moved it hashes the bytes, and only a different hash rebuilds. The
//! rebuild (parse, expand roots, index, start a watcher) runs on a blocking
//! thread while the old state keeps serving, and the swap happens once the new
//! state has its first snapshot. A config that does not parse, or whose roots do
//! not resolve, is refused: the old state keeps serving, the reason is logged to
//! stderr and recorded for `mesh-mcp doctor`.

use arc_swap::ArcSwap;
use mesh_core::config_watch::{
    config_fingerprint_cheap, stamp_config, write_daemon_config_status, DaemonConfigStatus,
};
use mesh_core::{expand_roots, AppState, AuditLogger, BackgroundRescanEngine};
use mesh_server::{FileWatcherService, WorkspaceIndexer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    /// `(mtime_ms, len)` of the file as last examined, whatever came of it.
    examined: Option<(u128, u64)>,
    /// SHA-256 of the config the serving state was built from.
    serving_sha: Option<String>,
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
    /// built-in default config: nothing to follow). `watcher_cancel` stops the
    /// watcher already running on `state`; it must be a child of `shutdown`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        state: Arc<AppState>,
        config_path: Option<PathBuf>,
        workspace_id: &str,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
        shutdown: CancellationToken,
        watcher_cancel: CancellationToken,
    ) -> Arc<Self> {
        let stamp = config_path.as_deref().and_then(|p| stamp_config(p).ok());
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
                examined: stamp.as_ref().map(|s| (s.mtime_ms, s.len)),
                serving_sha: stamp.map(|s| s.sha256),
                watcher_cancel,
                last_error: None,
            }),
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
                examined: None,
                serving_sha: None,
                watcher_cancel: watcher,
                last_error: None,
            }),
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
        let moved = {
            let book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            config_fingerprint_cheap(path) != book.examined
                && config_fingerprint_cheap(path).is_some()
        };
        if !moved || self.reloading.swap(true, Ordering::AcqRel) {
            return;
        }
        let host = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            host.reload_if_changed();
            host.reloading.store(false, Ordering::Release);
        });
    }

    /// Records which config this daemon serves, for `mesh-mcp doctor`.
    pub fn publish_status(&self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        let book = self.book.lock().unwrap_or_else(|p| p.into_inner());
        write_daemon_config_status(
            &self.workspace_id,
            &DaemonConfigStatus {
                started_ms: self.started_ms,
                config_path: Some(path),
                config_sha256: book.serving_sha.clone(),
                last_error: book.last_error.clone(),
            },
        );
    }

    fn reject(&self, examined: Option<(u128, u64)>, message: String) -> ReloadOutcome {
        tracing::warn!(target: "meshd::config", "{message}");
        {
            let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            book.examined = examined;
            book.last_error = Some(message.clone());
        }
        self.publish_status();
        ReloadOutcome::Rejected(message)
    }

    /// Compares the config file with the one the serving state was built from and
    /// rebuilds when its content differs. Blocking (parses and indexes the
    /// workspace); the old state serves until the new one has its first snapshot.
    pub fn reload_if_changed(&self) -> ReloadOutcome {
        let Some(path) = self.config_path.as_deref() else {
            return ReloadOutcome::Unchanged;
        };
        let Ok(_gate) = self.gate.try_lock() else {
            return ReloadOutcome::Unchanged;
        };
        let Some(cheap) = config_fingerprint_cheap(path) else {
            // Vanished (an editor's atomic save, or deleted): keep serving.
            return ReloadOutcome::Unchanged;
        };
        if self.book.lock().unwrap_or_else(|p| p.into_inner()).examined == Some(cheap) {
            return ReloadOutcome::Unchanged;
        }
        let shown = path.display();
        let stamp = match stamp_config(path) {
            Ok(s) => s,
            Err(e) => {
                return self.reject(
                    Some(cheap),
                    format!("cannot read config {shown}: {e}; still serving the previous config"),
                )
            }
        };
        let unchanged = {
            let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            book.examined = Some(cheap);
            let same = book.serving_sha.as_deref() == Some(stamp.sha256.as_str());
            if same && book.last_error.take().is_some() {
                drop(book);
                self.publish_status();
            }
            same
        };
        if unchanged {
            return ReloadOutcome::Unchanged;
        }
        let (config, base_dir) = match WorkspaceIndexer::load_config_at(path) {
            Ok(loaded) => loaded,
            Err(e) => {
                return self.reject(
                    Some(cheap),
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
                    Some(cheap),
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
        ingest_blocking(&state, &base_dir);
        let watcher_cancel = self.shutdown.child_token();
        start_watcher_blocking(&state, watcher_cancel.clone());

        self.current.store(Arc::clone(&state));
        let old_watcher = {
            let mut book = self.book.lock().unwrap_or_else(|p| p.into_inner());
            book.serving_sha = Some(stamp.sha256);
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
        let (cfg, base) = WorkspaceIndexer::load_config_at(&config).expect("load");
        let resolved = expand_roots(&cfg.workspace.roots, &base, &cfg.workspace.workspace_root)
            .expect("roots");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, resolved, audit.clone(), rescan.clone()));
        ingest_blocking(&state, &base);
        let shutdown = CancellationToken::new();
        let host = WorkspaceHost::new(
            state,
            Some(config.clone()),
            "test-ws-host",
            audit,
            rescan,
            shutdown.clone(),
            shutdown.child_token(),
        );
        Fixture {
            _dir: dir,
            root,
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
}
