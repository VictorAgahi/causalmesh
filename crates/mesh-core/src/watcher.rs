use crate::crawler::{ExcludeMatcher, FilesystemCrawler};
use crate::state::AppState;
use notify::{Config as NotifyConfig, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_mini::{
    new_debouncer, new_debouncer_opt, Config as DebouncerConfig, DebouncedEvent, Debouncer,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

mod git;

use git::{
    classify_git_event, DirObservation, GateAction, GitDir, GitEventKind, GitGate, LockTracker,
    StepInput,
};

/// Reload routine invoked on the background pool after a relevant filesystem change,
/// given the specific paths the watcher observed change (coalesced across every
/// burst folded into this one run — see `schedule_reload`) so it can index those
/// paths directly instead of re-crawling the whole tree to rediscover them. Kept
/// as a callback because the concrete indexer lives in `mesh-parsers`, which
/// `mesh-core` cannot depend on.
pub type ReloadFn = Arc<dyn Fn(&AppState, &[PathBuf]) + Send + Sync>;

/// Either watcher backend `spawn` can end up running: native watches (FSEvents recursive per
/// root on macOS; inotify/ReadDirectoryChangesW per directory elsewhere), or a polling backend —
/// on Linux/Windows when a workspace has more watchable directories than
/// `FileWatcherService::MAX_WATCHED_DIRS` (native registration has a real per-process/per-user
/// OS ceiling, Linux's `fs.inotify.max_user_watches` most concretely), on macOS only for a root
/// FSEvents refuses to watch.
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

/// How the native backend covers the tree, decided once per `spawn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchMode {
    /// macOS: one recursive FSEvents stream per root. Excluded directories *are* reported by the
    /// kernel, so `.gitignore`/`exclude_patterns` are applied in memory to every event.
    Recursive,
    /// Linux/Windows: one non-recursive watch per non-excluded directory (P2 step 3.3), new
    /// directories registered dynamically. Excluded directories are never watched at all.
    PerDirectory,
    /// Polling fallback: recursive per root, filtered in memory like `Recursive`.
    Poll,
}

/// Roots with a Git operation in progress, keyed by `AppState` address. Written only by
/// watcher threads on hold transitions; read by every tool call through
/// `FileWatcherService::git_operation_note`, which skips the lock entirely while
/// `ACTIVE_GIT_HOLDS` is zero (the normal case).
static ACTIVE_GIT_HOLDS: AtomicUsize = AtomicUsize::new(0);
static GIT_HOLDS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();

fn state_key(state: &AppState) -> usize {
    state as *const AppState as usize
}

fn set_git_hold(key: usize, holding: bool) {
    let mut holds = GIT_HOLDS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let changed = if holding {
        holds.insert(key)
    } else {
        holds.remove(&key)
    };
    if changed {
        if holding {
            ACTIVE_GIT_HOLDS.fetch_add(1, Ordering::AcqRel);
        } else {
            ACTIVE_GIT_HOLDS.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Clears this watcher's hold flag however its thread exits (cancel, disconnect, panic), so a
/// stopped watcher can never leave every later tool answer flagged "Git operation in progress".
struct HoldFlagGuard(usize);

impl Drop for HoldFlagGuard {
    fn drop(&mut self) {
        set_git_hold(self.0, false);
    }
}

/// Root-level ignore rules (`.gitignore`, `.ignore`, `.git/info/exclude`) compiled once per root
/// for the in-memory event filter, recompiled when one of those files changes.
fn compile_root_ignore(
    root: &Path,
    git_dir: Option<&GitDir>,
) -> Option<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let mut had_source = false;
    let exclude = git_dir.map(|d| d.common_dir.join("info").join("exclude"));
    for p in [root.join(".gitignore"), root.join(".ignore")]
        .into_iter()
        .chain(exclude)
    {
        if p.is_file() {
            if builder.add(&p).is_some() {
                // Unparseable: filter nothing on its account; `reload_paths` still decides.
                return None;
            }
            had_source = true;
        }
    }
    if !had_source {
        return None;
    }
    builder.build().ok()
}

/// In-memory event filter for the recursive backends (macOS FSEvents, polling): the same
/// `exclude_patterns` matcher the crawl uses, plus each root's own ignore files. Conservative by
/// construction — it only drops an event a full crawl would certainly skip; anything it can't
/// decide cheaply is kept and left to `WorkspaceIndexer::reload_paths`, which applies the
/// authoritative per-path exclusion anyway.
struct EventFilter {
    /// Sorted longest first, so the first `starts_with` hit is the most specific root.
    roots: Vec<(
        PathBuf,
        Option<ignore::gitignore::Gitignore>,
        Option<GitDir>,
    )>,
    matcher: ExcludeMatcher,
}

impl EventFilter {
    fn new(roots: &[PathBuf], git_dirs: &[Option<GitDir>], matcher: ExcludeMatcher) -> Self {
        let mut roots: Vec<_> = roots
            .iter()
            .zip(git_dirs)
            .map(|(r, g)| (r.clone(), compile_root_ignore(r, g.as_ref()), g.clone()))
            .collect();
        roots.sort_by_key(|(r, _, _)| std::cmp::Reverse(r.as_os_str().len()));
        Self { roots, matcher }
    }

    /// Recompiles a root's ignore rules when `path` is one of its ignore files.
    fn refresh_if_ignore_file(&mut self, path: &Path) {
        for (root, ignore, git_dir) in &mut self.roots {
            let is_ignore_file = path == root.join(".gitignore")
                || path == root.join(".ignore")
                || git_dir
                    .as_ref()
                    .is_some_and(|g| path == g.common_dir.join("info").join("exclude"));
            if is_ignore_file {
                *ignore = compile_root_ignore(root, git_dir.as_ref());
            }
        }
    }

    fn keeps(&self, path: &Path) -> bool {
        let Some((root, ignore, _)) = self.roots.iter().find(|(r, _, _)| path.starts_with(r))
        else {
            return false;
        };
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return true;
        }
        if self.matcher.is_excluded_with_root(rel, Some(root)) {
            return false;
        }
        let Some(ignore) = ignore else {
            return true;
        };
        if !ignore.matched_path_or_any_parents(path, false).is_ignore() {
            return true;
        }
        // A nested ignore file between `root` and `path` could re-include it (`!pattern`);
        // only a full crawl stacks those correctly, so such an event is kept, not guessed.
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == root.as_path() || !d.starts_with(root) {
                break;
            }
            if d.join(".gitignore").is_file() || d.join(".ignore").is_file() {
                return true;
            }
            dir = d.parent();
        }
        false
    }
}

/// In-kernel filesystem watcher service providing debounced change notifications
/// and atomic snapshot reloading per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(150);

    /// How long the tree must stay quiet after a Git operation — no live `index.lock`, no
    /// `rebase-*` directory, `HEAD` unchanged, no new event — before the held paths are
    /// released. Two debounce intervals: events reach this loop one debounce interval after
    /// they happen, so the last write of the operation is guaranteed to have been delivered
    /// (and folded into the hold) one interval before the window closes.
    pub const GIT_SETTLE: Duration = Duration::from_millis(300);

    /// Event-loop tick while a Git hold is open (the gate's release condition is time-based,
    /// so it must be re-evaluated even when no event arrives). Idle ticks stay at 300 ms.
    const GIT_HOLD_TICK: Duration = Duration::from_millis(50);
    const IDLE_TICK: Duration = Duration::from_millis(300);

    /// Linux/Windows only: above this many individually watchable directories across all roots,
    /// `spawn` gives up on per-directory native registration and falls back to
    /// `POLL_FALLBACK_INTERVAL` polling (P2 step 3.3). The cap protects against
    /// `fs.inotify.max_user_watches`, which some CI/container images and older distro defaults
    /// leave far below the 8192+ many desktop distros now ship.
    ///
    /// macOS has no such cap any more (Plan 4 step 4.2): it used to be 200, because `notify`'s
    /// FSEvents backend stops and restarts its whole event stream on every `.watch()` call
    /// (`FsEventWatcher::stop()` busy-waits for the stream's runloop to go idle — measured at
    /// over 11 s per call under load), so per-directory registration was unaffordable and any
    /// workspace past 200 directories fell back to a `PollWatcher` re-walking the tree forever.
    /// FSEvents is recursive by nature: one `.watch()` per root now covers the whole tree, and
    /// the exclusion filter runs in memory on each event instead (`EventFilter`).
    pub const MAX_WATCHED_DIRS: usize = 2048;

    /// Poll interval used only by the polling fallback. Coarser than `DEBOUNCE_INTERVAL` on
    /// purpose: polling a huge tree every 150ms would itself be real, avoidable CPU/IO cost.
    pub const POLL_FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

    /// The note prepended to a tool's text result while a Git operation holds reloads back
    /// (Plan 4 step 4.2): the answer still comes, immediately, from the last complete snapshot,
    /// and says which one. `None` when no operation is in progress for this `state`.
    pub fn git_operation_note(state: &AppState) -> Option<String> {
        if ACTIVE_GIT_HOLDS.load(Ordering::Acquire) == 0 {
            return None;
        }
        let holding = GIT_HOLDS
            .get()?
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&state_key(state));
        holding.then(|| {
            format!(
                "> **Git operation in progress**: reindexing is paused until it finishes; \
                 these results come from the last complete index (generation {}).\n\n",
                state.snapshot().generation
            )
        })
    }

    /// Walks `walk_root` for directories to individually watch, with every `exclude_patterns`
    /// check anchored to `matcher_root` rather than `walk_root` itself (see
    /// `FilesystemCrawler::plan_watch_dirs_from`'s doc for why the two must be kept separate for
    /// dynamic re-registration). Consumes from `budget` (shared across every root in one `spawn`
    /// call, so the *combined* directory count across all roots is what's capped against
    /// `MAX_WATCHED_DIRS`, not each root independently).
    fn plan_root(
        walk_root: &Path,
        matcher_root: &Path,
        matcher: &ExcludeMatcher,
        budget: usize,
    ) -> crate::crawler::WatchPlan {
        FilesystemCrawler::plan_watch_dirs_from(walk_root, matcher_root, matcher, Some(10), budget)
    }

    /// Watch targets for one resolved Git directory, for a backend that does not already cover
    /// it through a recursive root watch: the Git directory itself, non-recursively (`HEAD`,
    /// `index.lock`, the creation of `rebase-merge/`/`rebase-apply/` all show up as entries of
    /// it — watching the `HEAD` *file* instead would lose the watch the first time Git replaces
    /// it by rename), the common directory when it differs (`packed-refs` of a linked worktree),
    /// and `refs/` with every subdirectory (`fetch`/`push`/`commit` update a file nested inside
    /// `refs/heads|remotes/…`). Found by a plain directory walk that deliberately bypasses
    /// `ExcludeMatcher`, which unconditionally excludes `.git` (Commandment 4). Depth-capped at
    /// 5; symlinks skipped.
    ///
    /// Honest limitation (per-directory backends only): `refs/` subdirectories created after
    /// startup (`git remote add`) are not watched until the next restart.
    fn git_watch_targets(dir: &GitDir, recursive: bool) -> Vec<(PathBuf, RecursiveMode)> {
        let mut targets = vec![(dir.git_dir.clone(), RecursiveMode::NonRecursive)];
        if dir.common_dir != dir.git_dir {
            targets.push((dir.common_dir.clone(), RecursiveMode::NonRecursive));
        }
        let refs = dir.common_dir.join("refs");
        if refs.is_dir() {
            if recursive {
                targets.push((refs, RecursiveMode::Recursive));
            } else {
                let mut dirs = vec![refs.clone()];
                Self::collect_subdirs(&refs, &mut dirs, 5);
                targets.extend(dirs.into_iter().map(|d| (d, RecursiveMode::NonRecursive)));
            }
        }
        targets
    }

    fn collect_subdirs(dir: &Path, out: &mut Vec<PathBuf>, depth_left: usize) {
        if depth_left == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                continue;
            }
            if path.is_dir() {
                out.push(path.clone());
                Self::collect_subdirs(&path, out, depth_left - 1);
            }
        }
    }

    /// Resolves every root's Git directory (`None` for a root outside any work tree) and the
    /// deduplicated list the gate tracks (several roots of one repository share one entry).
    fn resolve_git_dirs(roots: &[PathBuf]) -> (Vec<Option<GitDir>>, Vec<GitDir>) {
        let per_root: Vec<Option<GitDir>> = roots.iter().map(|r| git::resolve_git_dir(r)).collect();
        let mut unique: Vec<GitDir> = Vec::new();
        for d in per_root.iter().flatten() {
            if !unique.iter().any(|u| u.git_dir == d.git_dir) {
                unique.push(d.clone());
            }
        }
        for d in &unique {
            tracing::info!(
                target: "mesh::watcher",
                "Git directory {} (common {}) tracked for in-progress operations.",
                d.git_dir.display(),
                d.common_dir.display()
            );
        }
        (per_root, unique)
    }

    fn poll_debouncer(
        tx: std::sync::mpsc::Sender<notify_debouncer_mini::DebounceEventResult>,
    ) -> Result<AnyDebouncer, notify::Error> {
        let config = DebouncerConfig::default()
            .with_timeout(Self::DEBOUNCE_INTERVAL)
            .with_notify_config(
                NotifyConfig::default().with_poll_interval(Self::POLL_FALLBACK_INTERVAL),
            );
        Ok(AnyDebouncer::Poll(new_debouncer_opt::<_, PollWatcher>(
            config, tx,
        )?))
    }

    /// Spawns the debounced file watcher actor in a background thread.
    ///
    /// Setup (watch registration, plus on Linux/Windows the per-root `plan_root` walk) runs
    /// synchronously on the calling thread, *before* returning — deliberately: callers (this
    /// crate's own tests, `mesh-server`'s `run_standalone`) rely on watches being live the
    /// instant this function returns; a file changed in the gap between `spawn()` returning and
    /// a background registration finishing would be silently missed (caught once by
    /// `test_file_watcher_live_reload` failing intermittently). `meshd`'s `main()` wraps this
    /// call in `tokio::task::spawn_blocking` so the synchronous work never blocks its runtime.
    ///
    /// Every debounced batch goes through, in this order (Plan 4 step 4.2):
    /// 1. events inside a resolved Git directory feed the Git gate — *before* any exclusion
    ///    filter, which ignores `.git/` unconditionally and would otherwise swallow the lock and
    ///    `HEAD` events;
    /// 2. working-tree events go through `is_relevant_path`, then (recursive backends only) the
    ///    in-memory `.gitignore`/`exclude_patterns` filter;
    /// 3. the gate decides: reload now, hold (a Git operation is in progress — tools keep
    ///    answering from the last complete snapshot, with a note), or flush everything held as
    ///    one grouped reload once the operation is over.
    pub fn spawn(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        reload: ReloadFn,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        let (tx, rx) = std::sync::mpsc::channel();

        let matcher = ExcludeMatcher::compile(&state.config.workspace.exclude_patterns);
        let existing_roots: Vec<PathBuf> = state
            .allowed_roots
            .iter()
            .filter(|r| r.exists())
            .cloned()
            .collect();
        let (root_git_dirs, git_dirs) = Self::resolve_git_dirs(&existing_roots);

        let mut backends: Vec<AnyDebouncer> = Vec::new();
        let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
        let mode;

        if cfg!(target_os = "macos") {
            mode = WatchMode::Recursive;
            let mut native =
                AnyDebouncer::Native(new_debouncer(Self::DEBOUNCE_INTERVAL, tx.clone())?);
            let mut refused: Vec<PathBuf> = Vec::new();
            for root in &existing_roots {
                match native.watcher().watch(root, RecursiveMode::Recursive) {
                    Ok(()) => tracing::info!(
                        target: "mesh::watcher",
                        "FileWatcher (FSEvents, recursive) watching root: {}",
                        root.display()
                    ),
                    Err(e) => {
                        tracing::warn!(
                            target: "mesh::watcher",
                            "FSEvents refused to watch root {} ({e}); falling back to polling \
                             every {:?} for this root only (network or unsupported volume?).",
                            root.display(),
                            Self::POLL_FALLBACK_INTERVAL,
                        );
                        refused.push(root.clone());
                    }
                }
            }
            for dir in &git_dirs {
                if existing_roots.iter().any(|r| dir.git_dir.starts_with(r)) {
                    continue; // already covered by that root's recursive stream
                }
                for (target, rec) in Self::git_watch_targets(dir, true) {
                    if let Err(e) = native.watcher().watch(&target, rec) {
                        tracing::warn!(target: "mesh::watcher", "Failed to watch {}: {e}", target.display());
                    }
                }
            }
            backends.push(native);
            if !refused.is_empty() {
                let mut poll = Self::poll_debouncer(tx)?;
                for root in &refused {
                    if let Err(e) = poll.watcher().watch(root, RecursiveMode::Recursive) {
                        tracing::warn!(target: "mesh::watcher", "Failed to poll root {}: {e}", root.display());
                    }
                }
                backends.push(poll);
            }
        } else {
            let mut per_root_dirs: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
            let mut capped = false;
            for root in &existing_roots {
                let budget = Self::MAX_WATCHED_DIRS.saturating_sub(watched_dirs.len());
                let plan = Self::plan_root(root, root, &matcher, budget);
                if plan.capped {
                    capped = true;
                    break;
                }
                watched_dirs.extend(plan.dirs.iter().cloned());
                per_root_dirs.push((root.clone(), plan.dirs));
            }

            if capped {
                mode = WatchMode::Poll;
                tracing::warn!(
                    target: "mesh::watcher",
                    "Workspace has more than {} watchable directories; falling back to polling \
                     every {:?} instead of native per-directory watches.",
                    Self::MAX_WATCHED_DIRS,
                    Self::POLL_FALLBACK_INTERVAL,
                );
                let mut poll = Self::poll_debouncer(tx)?;
                // One recursive watch per root. A failing root is logged and skipped rather
                // than aborting every other root's watch.
                for root in &existing_roots {
                    if let Err(e) = poll.watcher().watch(root, RecursiveMode::Recursive) {
                        tracing::warn!(target: "mesh::watcher", "Failed to watch root {}: {e}", root.display());
                        continue;
                    }
                    tracing::info!(target: "mesh::watcher", "FileWatcher (polling) watching root: {}", root.display());
                }
                for dir in &git_dirs {
                    if existing_roots.iter().any(|r| dir.git_dir.starts_with(r)) {
                        continue;
                    }
                    for (target, rec) in Self::git_watch_targets(dir, true) {
                        if let Err(e) = poll.watcher().watch(&target, rec) {
                            tracing::warn!(target: "mesh::watcher", "Failed to watch {}: {e}", target.display());
                        }
                    }
                }
                backends.push(poll);
            } else {
                mode = WatchMode::PerDirectory;
                let mut native = AnyDebouncer::Native(new_debouncer(Self::DEBOUNCE_INTERVAL, tx)?);
                for (root, dirs) in &per_root_dirs {
                    for dir in dirs {
                        if let Err(e) = native.watcher().watch(dir, RecursiveMode::NonRecursive) {
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
                }
                // `.git` is unconditionally excluded by `ExcludeMatcher`, so `plan_root` never
                // returns anything under it: the Git gate's targets are registered explicitly.
                for dir in &git_dirs {
                    for (target, rec) in Self::git_watch_targets(dir, false) {
                        if let Err(e) = native.watcher().watch(&target, rec) {
                            tracing::warn!(target: "mesh::watcher", "Failed to watch {}: {e}", target.display());
                        }
                    }
                }
                backends.push(native);
            }
        }

        let initial_heads: Vec<Option<Vec<u8>>> =
            git_dirs.iter().map(|d| git::probe(d).head).collect();
        let gate = GitGate::new(git_dirs, initial_heads, Self::GIT_SETTLE);
        let filter = (mode != WatchMode::PerDirectory)
            .then(|| EventFilter::new(&existing_roots, &root_git_dirs, matcher.clone()));

        // `Arc<Mutex<_>>`, not owned outright by the loop thread: only used by the
        // per-directory mode's dynamic registration, whose `.watch()` calls are dispatched to a
        // detached thread (see the dispatch site) so they never block the event-receive loop.
        let backends_keepalive = Arc::new(Mutex::new(backends));

        let handle = std::thread::Builder::new()
            .name("mesh-file-watcher".to_string())
            .spawn(move || {
                let mut ctx = LoopCtx {
                    state,
                    reload,
                    mode,
                    matcher,
                    watched_dirs,
                    gate,
                    lock_trackers: Vec::new(),
                    filter,
                    backends: backends_keepalive,
                    hold_guard: None,
                };
                ctx.lock_trackers
                    .resize_with(ctx.gate.dirs().len(), LockTracker::default);
                ctx.run(&rx, &cancel_token);
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
            let before = job_state.snapshot().generation;
            reload(&job_state, &drained);
            // Opt-in (`RUST_LOG=mesh::watcher=debug`) because fingerprinting hashes the whole
            // snapshot: lets `scripts/test_git_storm.sh` compare the watcher-installed index
            // with a cold `mesh-mcp graph --format fingerprint` of the same tree.
            if tracing::enabled!(target: "mesh::watcher", tracing::Level::DEBUG) {
                let snap = job_state.snapshot();
                if snap.generation != before {
                    tracing::debug!(
                        target: "mesh::watcher",
                        "Installed generation {} fingerprint {}",
                        snap.generation,
                        snap.fingerprint().combined
                    );
                }
            }
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

/// Everything the watcher thread owns. Split out of `spawn` so each stage of the per-batch
/// pipeline (classify → gate → reload) is a named method rather than one nested closure.
struct LoopCtx {
    state: Arc<AppState>,
    reload: ReloadFn,
    mode: WatchMode,
    matcher: ExcludeMatcher,
    watched_dirs: HashSet<PathBuf>,
    gate: GitGate,
    lock_trackers: Vec<LockTracker>,
    filter: Option<EventFilter>,
    backends: Arc<Mutex<Vec<AnyDebouncer>>>,
    hold_guard: Option<HoldFlagGuard>,
}

impl LoopCtx {
    fn run(
        &mut self,
        rx: &std::sync::mpsc::Receiver<notify_debouncer_mini::DebounceEventResult>,
        cancel_token: &CancellationToken,
    ) {
        loop {
            if cancel_token.is_cancelled() {
                tracing::info!(target: "mesh::watcher", "FileWatcher received cancellation signal, stopping.");
                break;
            }
            let holding = self.gate.is_holding();
            let tick = if holding {
                FileWatcherService::GIT_HOLD_TICK
            } else {
                FileWatcherService::IDLE_TICK
            };
            let (input, event_count) = match rx.recv_timeout(tick) {
                Ok(Ok(events)) => {
                    if self.mode == WatchMode::PerDirectory {
                        self.register_new_dirs(&events);
                    }
                    (self.classify(&events), events.len())
                }
                Ok(Err(errs)) => {
                    tracing::warn!(target: "mesh::watcher", "File watch error: {:?}", errs);
                    (StepInput::default(), 0)
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (StepInput::default(), 0),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            self.apply(input, event_count);
        }
    }

    /// Stage 1 + 2 of the pipeline documented on `FileWatcherService::spawn`.
    fn classify(&mut self, events: &[DebouncedEvent]) -> StepInput {
        let mut input = StepInput::default();
        let dirs = self.gate.dirs();
        for ev in events {
            let path = &ev.path;
            // Git directories first, before any exclusion filter. A path under some tracked
            // `git_dir` belongs to that directory even if it is also under another tracked
            // directory's `common_dir` (a main work tree and one of its linked worktrees both
            // configured as roots).
            let owner = dirs
                .iter()
                .position(|d| path.starts_with(&d.git_dir))
                .or_else(|| dirs.iter().position(|d| path.starts_with(&d.common_dir)));
            if let Some(i) = owner {
                match dirs.get(i).and_then(|d| classify_git_event(d, path)) {
                    Some(GitEventKind::RefChange) => {
                        input.active_dirs.push(i);
                        input.ref_dirs.push(i);
                    }
                    Some(GitEventKind::Activity) => input.active_dirs.push(i),
                    Some(GitEventKind::Noise) | None => {}
                }
                continue;
            }
            if let Some(filter) = self.filter.as_mut() {
                filter.refresh_if_ignore_file(path);
            }
            if !FileWatcherService::is_relevant_path(path) {
                continue;
            }
            if let Some(filter) = self.filter.as_ref() {
                if !filter.keeps(path) {
                    continue;
                }
            }
            input.paths.push(path.clone());
        }
        input.active_dirs.dedup();
        input.ref_dirs.dedup();
        input
    }

    /// Fresh filesystem observation of every tracked Git directory for the gate.
    fn observe(&mut self) -> Vec<DirObservation> {
        let now = Instant::now();
        let now_wall = SystemTime::now();
        let mut check = git::user_git_running;
        self.gate
            .dirs()
            .iter()
            .zip(self.lock_trackers.iter_mut())
            .map(|(dir, tracker)| {
                let probe = git::probe(dir);
                let lock_live =
                    tracker.lock_is_live(&dir.git_dir, probe.lock_mtime, now_wall, now, &mut check);
                DirObservation {
                    busy: lock_live || probe.sequence_in_progress,
                    head: probe.head,
                }
            })
            .collect()
    }

    /// Stage 3: let the gate decide, and act on its answer.
    fn apply(&mut self, input: StepInput, event_count: usize) {
        let idle_batch =
            input.paths.is_empty() && input.active_dirs.is_empty() && input.ref_dirs.is_empty();
        if idle_batch && !self.gate.is_holding() {
            // Nothing relevant and nothing held: no probe, no syscalls — this is the steady
            // state of an idle workspace (and of a build writing only excluded outputs).
            return;
        }
        let observations = if self.gate.dirs().is_empty() {
            Vec::new()
        } else {
            self.observe()
        };
        let relevant = input.paths.len();
        match self.gate.step(Instant::now(), input, &observations) {
            GateAction::Pass(paths) => {
                if paths.is_empty() {
                    return;
                }
                tracing::info!(
                    target: "mesh::watcher",
                    "Detected filesystem mutations ({relevant} relevant of {event_count} events). Scheduling atomic graph rescan."
                );
                FileWatcherService::schedule_reload(
                    self.state.clone(),
                    self.reload.clone(),
                    &paths,
                );
            }
            GateAction::Held { started: true } => {
                let key = state_key(&self.state);
                set_git_hold(key, true);
                self.hold_guard = Some(HoldFlagGuard(key));
                tracing::info!(
                    target: "mesh::watcher",
                    "Git operation in progress: holding reloads (tools answer from generation {}).",
                    self.state.snapshot().generation
                );
            }
            GateAction::Held { started: false } => {}
            GateAction::Flush { paths, timed_out } => {
                self.hold_guard = None;
                if timed_out {
                    tracing::warn!(
                        target: "mesh::watcher",
                        "Git operation still in progress after {:?}: reloading the {} path(s) held so far anyway.",
                        git::MAX_HOLD,
                        paths.len()
                    );
                } else {
                    tracing::info!(
                        target: "mesh::watcher",
                        "Git operation finished: releasing {} held path(s) as one reload.",
                        paths.len()
                    );
                }
                if !paths.is_empty() {
                    FileWatcherService::schedule_reload(
                        self.state.clone(),
                        self.reload.clone(),
                        &paths,
                    );
                }
            }
        }
    }

    /// Per-directory mode only: a directory this run didn't know about at startup (created
    /// after `spawn`, e.g. `mkdir`, a branch checkout, an archive extraction) has no watch yet
    /// — per-directory registration doesn't track new subdirectories automatically. Any event
    /// path that is now a directory, isn't already watched, and isn't itself excluded gets a
    /// fresh watch; `plan_root` also picks up its qualifying subdirectories in case a whole
    /// subtree appeared in a single burst.
    fn register_new_dirs(&mut self, events: &[DebouncedEvent]) {
        for ev in events {
            if self.watched_dirs.len() >= FileWatcherService::MAX_WATCHED_DIRS {
                tracing::warn!(
                    target: "mesh::watcher",
                    "Reached {} watched directories; new subdirectories under {} will not be \
                     individually watched until the next restart.",
                    FileWatcherService::MAX_WATCHED_DIRS,
                    ev.path.display()
                );
                break;
            }
            if self.watched_dirs.contains(&ev.path) || !ev.path.is_dir() {
                continue;
            }
            // Accepted inefficiency, not a correctness gap: if a parent and child directory
            // both appear in the same batch and the child is iterated first, its walk
            // re-enumerates a subtree the parent's walk enumerates again; `watched_dirs`'s
            // `insert`-based dedup still registers each directory exactly once.
            let Some(root) = self
                .state
                .allowed_roots
                .iter()
                .find(|r| ev.path.starts_with(r.as_path()))
                .cloned()
            else {
                continue;
            };
            let budget = FileWatcherService::MAX_WATCHED_DIRS - self.watched_dirs.len();
            // Enumeration is cheap (a directory walk, no OS watch API) and stays synchronous so
            // `watched_dirs` bookkeeping never races a second event for the same directory.
            let plan = FileWatcherService::plan_root(&ev.path, &root, &self.matcher, budget);
            if plan.capped {
                tracing::warn!(
                    target: "mesh::watcher",
                    "New subtree under {} alone exceeds the remaining watch budget ({budget}); \
                     none of it will be individually watched until the next restart.",
                    ev.path.display()
                );
                continue;
            }
            let new_dirs: Vec<PathBuf> = plan
                .dirs
                .into_iter()
                .filter(|d| self.watched_dirs.insert(d.clone()))
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
            // A plain detached thread, deliberately *not* `state.rescan.spawn`: that pool is
            // sized as small as 1 thread and runs the real reload jobs; a slow `.watch()` call
            // there would starve them. Threads only serialize against each other through the
            // backends mutex (known limitation: nothing bounds how many can be in flight during
            // a long burst of separate new-directory windows).
            let backends = self.backends.clone();
            std::thread::spawn(move || {
                let mut guard = backends.lock().unwrap_or_else(|p| p.into_inner());
                let Some(backend) = guard.first_mut() else {
                    return;
                };
                for dir in &new_dirs {
                    if let Err(e) = backend.watcher().watch(dir, RecursiveMode::NonRecursive) {
                        tracing::warn!(target: "mesh::watcher", "Failed to watch new directory {}: {e}", dir.display());
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for a real bug caught by `/code-review`: `.git` is unconditionally excluded
    /// by `ExcludeMatcher`, so `plan_root`'s walk never returns anything under it — the
    /// per-directory backend needs `git_watch_targets` to explicitly cover the Git directory
    /// and every `refs/` subdirectory, or a `fetch`/`push`/branch update (which touches a file
    /// under `refs/heads|remotes/...`) would silently never trigger a reload. Since 4.2 the Git
    /// directory itself is watched (not the `HEAD` file, whose watch Git's rename-replace would
    /// drop), so `index.lock` and `rebase-*` creation are seen too.
    #[test]
    fn git_watch_targets_covers_git_dir_and_every_refs_subdirectory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::create_dir_all(root.join(".git/refs/heads")).expect("mkdir heads");
        std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).expect("mkdir remotes");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        std::fs::write(root.join(".git/refs/heads/main"), "deadbeef\n").expect("write ref");

        let dir = git::resolve_git_dir(&root).expect("git dir");
        let targets: Vec<PathBuf> = FileWatcherService::git_watch_targets(&dir, false)
            .into_iter()
            .map(|(p, _)| p)
            .collect();

        assert!(targets.contains(&root.join(".git")));
        assert!(targets.contains(&root.join(".git/refs")));
        assert!(
            targets.contains(&root.join(".git/refs/heads")),
            "a fetch/push/commit updates a file inside refs/heads, not refs/heads itself — that \
             directory needs its own watch: {targets:?}"
        );
        assert!(targets.contains(&root.join(".git/refs/remotes")));
        assert!(
            targets.contains(&root.join(".git/refs/remotes/origin")),
            "nested remote directories must be covered too: {targets:?}"
        );

        let recursive = FileWatcherService::git_watch_targets(&dir, true);
        assert!(recursive.contains(&(root.join(".git/refs"), RecursiveMode::Recursive)));
    }

    #[test]
    fn no_git_dir_for_a_non_git_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        // The temp dir must not itself sit inside a work tree for this to mean anything.
        if root.ancestors().any(|a| a.join(".git").exists()) {
            return;
        }
        let (per_root, unique) = FileWatcherService::resolve_git_dirs(&[root]);
        assert_eq!(per_root, vec![None]);
        assert!(unique.is_empty());
    }

    /// The macOS/polling in-memory filter: `exclude_patterns` and the root's `.gitignore` drop
    /// events, a nested ignore file (which might re-include) keeps them for `reload_paths` to
    /// decide, and an edited `.gitignore` is picked up.
    #[test]
    fn event_filter_applies_excludes_and_root_gitignore_conservatively() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::write(root.join(".gitignore"), "dist/\n*.gen.ts\n").expect("gitignore");
        std::fs::create_dir_all(root.join("pkg")).expect("mkdir");
        std::fs::write(root.join("pkg/.gitignore"), "!keep.gen.ts\n").expect("nested");
        let matcher = ExcludeMatcher::compile(&["vendor/**".to_string()]);
        let mut f = EventFilter::new(&[root.clone()], &[None], matcher);

        assert!(f.keeps(&root.join("src/a.ts")));
        assert!(!f.keeps(&root.join("vendor/x/a.go")), "exclude_patterns");
        assert!(
            !f.keeps(&root.join("dist/a.ts")),
            "root .gitignore dir rule"
        );
        assert!(
            !f.keeps(&root.join("src/b.gen.ts")),
            "root .gitignore file rule"
        );
        assert!(
            f.keeps(&root.join("pkg/keep.gen.ts")),
            "nested ignore file: kept"
        );
        assert!(!f.keeps(&root.join(".git/HEAD")), ".git is always excluded");
        assert!(!f.keeps(Path::new("/elsewhere/a.ts")), "outside every root");

        std::fs::write(root.join(".gitignore"), "").expect("gitignore");
        f.refresh_if_ignore_file(&root.join(".gitignore"));
        assert!(
            f.keeps(&root.join("dist/a.ts")),
            "recompiled after the edit"
        );
    }

    /// The tool-side note is keyed per `AppState`, present only while that state's watcher
    /// holds, and names the generation being served.
    #[test]
    fn git_operation_note_follows_the_hold_flag() {
        let a = test_state();
        let b = test_state();
        assert_eq!(FileWatcherService::git_operation_note(&a), None);
        {
            let key = state_key(&a);
            set_git_hold(key, true);
            let _guard = HoldFlagGuard(key);
            let note = FileWatcherService::git_operation_note(&a).expect("note while holding");
            assert!(note.contains("Git operation in progress"), "{note}");
            assert!(note.contains("generation 0"), "{note}");
            assert_eq!(
                FileWatcherService::git_operation_note(&b),
                None,
                "other state"
            );
        }
        assert_eq!(
            FileWatcherService::git_operation_note(&a),
            None,
            "guard cleared it"
        );
    }

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
