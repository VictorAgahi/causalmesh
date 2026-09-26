//! Git-operation gate for the file watcher (Plan 4 step 4.2).
//!
//! A reload that runs while `git checkout`/`rebase`/`merge` is still rewriting the working tree
//! reads a half-old, half-new disk and installs a snapshot that mixes both branches. This module
//! detects "a Git operation is in progress" for every configured root and makes the watcher hold
//! the paths it sees until the operation is over, then release them as **one** grouped reload.
//!
//! Split in three layers so the decision logic is testable without a real filesystem:
//! - [`resolve_git_dir`]: finds the real Git directory of a root (following a `gitdir:` pointer
//!   when `.git` is a file — submodules and `git worktree`).
//! - [`GitGate`]: the pure state machine (no I/O). It is fed [`DirObservation`]s and the paths of
//!   each debounced batch, and answers with a [`GateAction`].
//! - [`LockTracker`] + [`probe`]: the filesystem side (stat `index.lock` and the rebase
//!   directories, read `HEAD`), including the orphan-lock rule (lock older than
//!   [`ORPHAN_LOCK_AGE`] **and** no `git` process of this user → ignored, with a `warn`).

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// An `index.lock` older than this is suspected orphaned (a `git` process that crashed or was
/// killed); only then is the process table consulted.
pub(crate) const ORPHAN_LOCK_AGE: Duration = Duration::from_secs(30);

/// Minimum spacing between two process-table checks for the same lock, so a suspected orphan
/// that turns out to belong to a long-running `git` (a huge checkout) never makes the watcher
/// fork `pgrep` on every tick.
pub(crate) const PROCESS_RECHECK: Duration = Duration::from_secs(10);

/// Hard upper bound on one hold. A rebase stopped on a conflict keeps `rebase-merge/` around for
/// as long as the user takes to resolve it; the index must not stay frozen for that long. Past
/// this bound the accumulated paths are reloaded anyway (with a `warn`), and a new hold starts if
/// the operation is still in progress.
pub(crate) const MAX_HOLD: Duration = Duration::from_secs(60);

/// Wall-clock budget for one `pgrep`/`ps` invocation.
const PROCESS_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// The Git directory of one or more watched roots, fully resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitDir {
    /// Where `HEAD`, `index`, `index.lock`, `rebase-merge/` and `rebase-apply/` live. For a
    /// linked worktree this is `<main>/.git/worktrees/<name>`, for a submodule usually
    /// `<super>/.git/modules/<name>`.
    pub git_dir: PathBuf,
    /// Where `refs/` and `packed-refs` live (`commondir`); equal to `git_dir` for a plain repo.
    pub common_dir: PathBuf,
    /// `<work tree top>/.git/HEAD`: the path handed to the reload routine when this directory's
    /// `HEAD` or refs changed. `WorkspaceIndexer::reload_paths` recognises a `.git/HEAD`
    /// component pair as "a ref moved" and runs a full reload; for a worktree or submodule the
    /// real `HEAD` lives elsewhere (`…/worktrees/<name>/HEAD`) and would not be recognised, so
    /// the gate always reports the logical `<top>/.git/HEAD` instead.
    pub head_marker: PathBuf,
}

/// Finds the Git directory governing `root`: the nearest ancestor (including `root` itself)
/// holding a `.git` entry. A `.git` directory is used as is; a `.git` *file* (submodule, linked
/// worktree) is followed through its `gitdir: <path>` line, relative paths being resolved
/// against the file's own directory. Only `<ancestor>/.git` is stat'ed on the way up — nothing
/// is listed or walked. Returns `None` when `root` is not inside a Git work tree or the pointer
/// is unreadable.
pub(crate) fn resolve_git_dir(root: &Path) -> Option<GitDir> {
    // Bounded like any other upward search in this crate: a pathological depth is not a repo.
    for top in root.ancestors().take(64) {
        let dot_git = top.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        let git_dir = if meta.is_dir() {
            dot_git.clone()
        } else if meta.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let pointer = content
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))?
                .trim();
            if pointer.is_empty() {
                return None;
            }
            let p = Path::new(pointer);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                top.join(p)
            }
        } else {
            // A symlinked `.git` is never followed (Commandment 4).
            return None;
        };
        let git_dir = dunce::canonicalize(&git_dir).ok()?;
        if !git_dir.is_dir() {
            return None;
        }
        let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
            .ok()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .and_then(|c| {
                let p = Path::new(&c);
                let p = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    git_dir.join(p)
                };
                dunce::canonicalize(p).ok()
            })
            .unwrap_or_else(|| git_dir.clone());
        return Some(GitDir {
            git_dir,
            common_dir,
            head_marker: top.join(".git").join("HEAD"),
        });
    }
    None
}

/// What one event inside a resolved Git directory means for the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitEventKind {
    /// Object-store churn (`objects/**`): a `fetch` or `gc` writes thousands of these and none
    /// of them changes the working tree. Dropped.
    Noise,
    /// Anything else Git writes (`index`, `index.lock`, `rebase-merge/…`, `ORIG_HEAD`, logs):
    /// an operation is running.
    Activity,
    /// `HEAD`, `refs/**` or `packed-refs`: a ref moved, the next reload must be a full one.
    RefChange,
}

/// Classifies `path` against `dir`; `None` when the path is outside both of its directories.
pub(crate) fn classify_git_event(dir: &GitDir, path: &Path) -> Option<GitEventKind> {
    let first = |rel: &Path| {
        rel.components().find_map(|c| match c {
            Component::Normal(s) => Some(s.to_os_string()),
            _ => None,
        })
    };
    if let Ok(rel) = path.strip_prefix(&dir.git_dir) {
        let head = first(rel);
        return Some(match head.as_deref().and_then(|s| s.to_str()) {
            Some("objects") => GitEventKind::Noise,
            Some("HEAD" | "refs" | "packed-refs") => GitEventKind::RefChange,
            _ => GitEventKind::Activity,
        });
    }
    if let Ok(rel) = path.strip_prefix(&dir.common_dir) {
        let head = first(rel);
        return Some(match head.as_deref().and_then(|s| s.to_str()) {
            Some("refs" | "packed-refs") => GitEventKind::RefChange,
            Some("objects") => GitEventKind::Noise,
            // Another worktree's private files (`worktrees/<other>/…`) or the main work
            // tree's index: not this root's business.
            _ => GitEventKind::Noise,
        });
    }
    None
}

/// Filesystem state of one Git directory, taken by [`probe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirProbe {
    /// `index.lock`'s modification time, when it exists.
    pub lock_mtime: Option<SystemTime>,
    /// `rebase-merge/` or `rebase-apply/` exists (rebase, `am`).
    pub sequence_in_progress: bool,
    /// Raw `HEAD` content (a `ref: …` line or a detached SHA).
    pub head: Option<Vec<u8>>,
}

/// Four syscalls: stat `index.lock`, stat the two rebase directories, read `HEAD` (≤ 64 bytes
/// in practice). Called once per debounced batch and on every gate tick while a hold is active —
/// never on an idle tick.
pub(crate) fn probe(dir: &GitDir) -> DirProbe {
    let lock_mtime = std::fs::metadata(dir.git_dir.join("index.lock"))
        .ok()
        .map(|m| m.modified().unwrap_or(SystemTime::UNIX_EPOCH));
    let sequence_in_progress =
        dir.git_dir.join("rebase-merge").is_dir() || dir.git_dir.join("rebase-apply").is_dir();
    let head = std::fs::read(dir.git_dir.join("HEAD")).ok();
    DirProbe {
        lock_mtime,
        sequence_in_progress,
        head,
    }
}

/// Orphan-lock bookkeeping for one Git directory (impure only through the `git_running`
/// callback, so the rule itself is unit-tested without spawning anything).
#[derive(Debug, Default)]
pub(crate) struct LockTracker {
    /// mtime of the lock already declared orphaned: ignored until it disappears or changes.
    orphan: Option<SystemTime>,
    last_check: Option<Instant>,
}

impl LockTracker {
    /// Whether a present `index.lock` means "an operation is running". A lock younger than
    /// [`ORPHAN_LOCK_AGE`] always does. An older one does unless `git_running` reports that
    /// this user has no `git` process at all (`Some(false)`), in which case it is declared
    /// orphaned, logged once, and ignored. `None` (the check itself failed or is unavailable
    /// on this platform) keeps the lock live — [`MAX_HOLD`] still bounds the wait.
    pub fn lock_is_live(
        &mut self,
        dir: &Path,
        lock_mtime: Option<SystemTime>,
        now_wall: SystemTime,
        now: Instant,
        git_running: &mut dyn FnMut() -> Option<bool>,
    ) -> bool {
        let Some(mtime) = lock_mtime else {
            self.orphan = None;
            self.last_check = None;
            return false;
        };
        if self.orphan == Some(mtime) {
            return false;
        }
        let age = now_wall.duration_since(mtime).unwrap_or(Duration::ZERO);
        if age < ORPHAN_LOCK_AGE {
            return true;
        }
        if self
            .last_check
            .is_some_and(|t| now.saturating_duration_since(t) < PROCESS_RECHECK)
        {
            return true;
        }
        self.last_check = Some(now);
        if git_running() == Some(false) {
            tracing::warn!(
                target: "mesh::watcher",
                "{}/index.lock is {}s old and no git process of this user is running: \
                 treating it as orphaned and resuming normal reloads (remove it once no git \
                 command is running).",
                dir.display(),
                age.as_secs()
            );
            self.orphan = Some(mtime);
            return false;
        }
        true
    }
}

/// Whether this user has at least one `git` process: `pgrep -u <uid> -x git` (exit 0 = yes,
/// 1 = no), falling back to `ps -u <uid> -o comm=` when `pgrep` is unavailable. Both run
/// without a shell and under [`PROCESS_CHECK_TIMEOUT`]; `None` when neither gives an answer.
/// Only ever called for a lock older than [`ORPHAN_LOCK_AGE`], at most every
/// [`PROCESS_RECHECK`].
#[cfg(unix)]
pub(crate) fn user_git_running() -> Option<bool> {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() }.to_string();
    let mut pgrep = std::process::Command::new("pgrep");
    pgrep.args(["-u", &uid, "-x", "git"]);
    if let Some((code, _)) = run_bounded(pgrep, false) {
        match code {
            Some(0) => return Some(true),
            Some(1) => return Some(false),
            _ => {}
        }
    }
    let mut ps = std::process::Command::new("ps");
    ps.args(["-u", &uid, "-o", "comm="]);
    let (code, out) = run_bounded(ps, true)?;
    if code != Some(0) {
        return None;
    }
    let text = String::from_utf8_lossy(&out);
    Some(text.lines().any(|l| {
        Path::new(l.trim())
            .file_name()
            .is_some_and(|n| n == std::ffi::OsStr::new("git"))
    }))
}

#[cfg(not(unix))]
pub(crate) fn user_git_running() -> Option<bool> {
    None
}

/// Spawns `cmd` (never through a shell), waits at most [`PROCESS_CHECK_TIMEOUT`], kills it past
/// that. Returns the exit code and, when `capture`, its stdout.
#[cfg(unix)]
fn run_bounded(mut cmd: std::process::Command, capture: bool) -> Option<(Option<i32>, Vec<u8>)> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    cmd.stdout(if capture {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().ok()?;
    // Drain stdout on a helper thread so a chatty `ps` can never fill the pipe and stall.
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let deadline = Instant::now() + PROCESS_CHECK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let status = status?;
    Some((status.code(), out))
}

/// One Git directory's state, as the pure gate sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DirObservation {
    /// A live (non-orphaned) `index.lock` exists, or a rebase/`am` sequence is in progress.
    pub busy: bool,
    /// Current `HEAD` content.
    pub head: Option<Vec<u8>>,
}

/// What the watcher must do after one [`GitGate::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateAction {
    /// No Git operation involved: reload these paths now (possibly empty).
    Pass(Vec<PathBuf>),
    /// An operation is in progress; the paths were accumulated. `started` is true on the step
    /// that opened the hold.
    Held { started: bool },
    /// The operation is over (or [`MAX_HOLD`] elapsed): reload everything accumulated, once.
    Flush {
        paths: Vec<PathBuf>,
        timed_out: bool,
    },
}

#[derive(Debug)]
struct Hold {
    started: Instant,
    last_activity: Instant,
    paths: Vec<PathBuf>,
    ref_dirty: Vec<bool>,
}

/// Pure state machine deciding when paths may be reloaded. One instance per watcher, covering
/// every resolved Git directory: a hold on any of them holds everything, since a full reload
/// triggered by another root would re-crawl the busy one mid-operation anyway.
#[derive(Debug)]
pub(crate) struct GitGate {
    dirs: Vec<GitDir>,
    last_head: Vec<Option<Vec<u8>>>,
    hold: Option<Hold>,
    settle: Duration,
}

/// One step's input.
#[derive(Debug, Default)]
pub(crate) struct StepInput {
    /// Relevant, non-excluded working-tree paths from this batch (empty on a tick).
    pub paths: Vec<PathBuf>,
    /// Index into `dirs` of every Git directory that had an `Activity`/`RefChange` event.
    pub active_dirs: Vec<usize>,
    /// Index into `dirs` of every Git directory that had a `RefChange` event.
    pub ref_dirs: Vec<usize>,
}

impl GitGate {
    /// `initial` holds each directory's `HEAD` at startup, so the first batch doesn't mistake
    /// "never read" for "changed". `settle` is how long the tree must stay quiet — no live lock,
    /// no sequence directory, no `HEAD` change, no new event — before a hold is released.
    pub fn new(dirs: Vec<GitDir>, initial: Vec<Option<Vec<u8>>>, settle: Duration) -> Self {
        let mut last_head = initial;
        last_head.resize(dirs.len(), None);
        Self {
            dirs,
            last_head,
            hold: None,
            settle,
        }
    }

    pub fn dirs(&self) -> &[GitDir] {
        &self.dirs
    }

    pub fn is_holding(&self) -> bool {
        self.hold.is_some()
    }

    /// Feeds one debounced batch (or, with an empty `input`, one tick) together with a fresh
    /// observation of every Git directory (same order as `dirs`).
    pub fn step(&mut self, now: Instant, input: StepInput, obs: &[DirObservation]) -> GateAction {
        let mut activity = !input.active_dirs.is_empty();
        let mut any_busy = false;
        let mut ref_dirty = vec![false; self.dirs.len()];
        for &i in &input.ref_dirs {
            if let Some(slot) = ref_dirty.get_mut(i) {
                *slot = true;
            }
        }
        for (i, o) in obs.iter().enumerate().take(self.dirs.len()) {
            any_busy |= o.busy;
            if self.last_head.get(i).is_some_and(|h| *h != o.head) {
                self.last_head[i] = o.head.clone();
                ref_dirty[i] = true;
                activity = true;
            }
        }
        activity |= any_busy || ref_dirty.iter().any(|d| *d);

        let Some(hold) = self.hold.as_mut() else {
            if !activity {
                return GateAction::Pass(input.paths);
            }
            self.hold = Some(Hold {
                started: now,
                last_activity: now,
                paths: input.paths,
                ref_dirty,
            });
            return GateAction::Held { started: true };
        };

        if activity || !input.paths.is_empty() {
            hold.last_activity = now;
        }
        hold.paths.extend(input.paths);
        for (slot, d) in hold.ref_dirty.iter_mut().zip(ref_dirty) {
            *slot |= d;
        }

        let settled = !any_busy && now.saturating_duration_since(hold.last_activity) >= self.settle;
        let timed_out = now.saturating_duration_since(hold.started) >= MAX_HOLD;
        if !settled && !timed_out {
            return GateAction::Held { started: false };
        }
        let Some(hold) = self.hold.take() else {
            return GateAction::Held { started: false };
        };
        let mut paths = hold.paths;
        for (dir, dirty) in self.dirs.iter().zip(hold.ref_dirty) {
            if dirty {
                paths.push(dir.head_marker.clone());
            }
        }
        GateAction::Flush {
            paths,
            timed_out: timed_out && !settled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> GitDir {
        let top = PathBuf::from(format!("/w/{name}"));
        GitDir {
            git_dir: top.join(".git"),
            common_dir: top.join(".git"),
            head_marker: top.join(".git/HEAD"),
        }
    }

    fn obs(busy: bool, head: &str) -> DirObservation {
        DirObservation {
            busy,
            head: Some(head.as_bytes().to_vec()),
        }
    }

    const SETTLE: Duration = Duration::from_millis(300);

    fn gate() -> GitGate {
        GitGate::new(vec![dir("a")], vec![Some(b"ref: main".to_vec())], SETTLE)
    }

    fn paths(p: &[&str]) -> Vec<PathBuf> {
        p.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn plain_edit_without_git_activity_passes_straight_through() {
        let mut g = gate();
        let t0 = Instant::now();
        let input = StepInput {
            paths: paths(&["/w/a/x.rs"]),
            ..Default::default()
        };
        assert_eq!(
            g.step(t0, input, &[obs(false, "ref: main")]),
            GateAction::Pass(paths(&["/w/a/x.rs"]))
        );
        assert!(!g.is_holding());
    }

    /// `git checkout`: the lock is taken, files are rewritten, the lock goes away, then `HEAD`
    /// is rewritten. Nothing may be released before `HEAD` has stayed put for the settle
    /// window, and everything must come out in one flush, with the `HEAD` marker (full reload).
    #[test]
    fn checkout_is_held_until_lock_gone_and_head_stable_then_flushed_once() {
        let mut g = gate();
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        // Worktree files delivered while index.lock exists.
        let a = g.step(
            ms(0),
            StepInput {
                paths: paths(&["/w/a/1.rs"]),
                ..Default::default()
            },
            &[obs(true, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: true });
        let a = g.step(
            ms(100),
            StepInput {
                paths: paths(&["/w/a/2.rs"]),
                active_dirs: vec![0],
                ..Default::default()
            },
            &[obs(true, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: false });
        // Lock released, HEAD not rewritten yet: a tick 1s later still sees the lock gone and
        // nothing else — but HEAD changes on the next observation.
        let a = g.step(ms(200), StepInput::default(), &[obs(false, "ref: main")]);
        assert_eq!(a, GateAction::Held { started: false });
        let a = g.step(ms(250), StepInput::default(), &[obs(false, "ref: feature")]);
        assert_eq!(a, GateAction::Held { started: false }, "HEAD just changed");
        let a = g.step(ms(400), StepInput::default(), &[obs(false, "ref: feature")]);
        assert_eq!(
            a,
            GateAction::Held { started: false },
            "HEAD stable < settle"
        );
        let a = g.step(ms(560), StepInput::default(), &[obs(false, "ref: feature")]);
        assert_eq!(
            a,
            GateAction::Flush {
                paths: paths(&["/w/a/1.rs", "/w/a/2.rs", "/w/a/.git/HEAD"]),
                timed_out: false
            }
        );
        assert!(!g.is_holding());
        // Back to normal afterwards.
        let a = g.step(
            ms(700),
            StepInput {
                paths: paths(&["/w/a/3.rs"]),
                ..Default::default()
            },
            &[obs(false, "ref: feature")],
        );
        assert_eq!(a, GateAction::Pass(paths(&["/w/a/3.rs"])));
    }

    /// A fast checkout can finish (lock gone) before its first worktree event is delivered.
    /// The `HEAD` change alone must still open a hold.
    #[test]
    fn head_change_without_lock_still_holds() {
        let mut g = gate();
        let t0 = Instant::now();
        let a = g.step(
            t0,
            StepInput {
                paths: paths(&["/w/a/1.rs"]),
                ..Default::default()
            },
            &[obs(false, "ref: other")],
        );
        assert_eq!(a, GateAction::Held { started: true });
        let a = g.step(
            t0 + SETTLE,
            StepInput::default(),
            &[obs(false, "ref: other")],
        );
        assert!(matches!(a, GateAction::Flush { ref paths, .. } if paths.len() == 2));
    }

    /// `git commit`: HEAD's content (`ref: refs/heads/main`) does not change, the branch ref
    /// does, and `index.lock` lives only a few milliseconds. The ref event alone must hold and
    /// then request a full reload through the marker.
    #[test]
    fn commit_ref_update_without_head_change_is_a_ref_change() {
        let mut g = gate();
        let t0 = Instant::now();
        let a = g.step(
            t0,
            StepInput {
                active_dirs: vec![0],
                ref_dirs: vec![0],
                ..Default::default()
            },
            &[obs(false, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: true });
        let a = g.step(
            t0 + SETTLE,
            StepInput::default(),
            &[obs(false, "ref: main")],
        );
        assert_eq!(
            a,
            GateAction::Flush {
                paths: paths(&["/w/a/.git/HEAD"]),
                timed_out: false
            }
        );
    }

    /// `git status` refreshing the index (lock + index write, nothing else): a short hold that
    /// flushes nothing — the watcher schedules no reload for an empty flush.
    #[test]
    fn index_refresh_only_flushes_nothing() {
        let mut g = gate();
        let t0 = Instant::now();
        g.step(
            t0,
            StepInput {
                active_dirs: vec![0],
                ..Default::default()
            },
            &[obs(false, "ref: main")],
        );
        let a = g.step(
            t0 + SETTLE,
            StepInput::default(),
            &[obs(false, "ref: main")],
        );
        assert_eq!(
            a,
            GateAction::Flush {
                paths: vec![],
                timed_out: false
            }
        );
    }

    /// Late worktree events (delivered after the lock is gone) keep the hold open instead of
    /// being split into a second reload.
    #[test]
    fn late_worktree_events_extend_the_settle_window() {
        let mut g = gate();
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        g.step(ms(0), StepInput::default(), &[obs(true, "ref: main")]);
        let a = g.step(
            ms(290),
            StepInput {
                paths: paths(&["/w/a/late.rs"]),
                ..Default::default()
            },
            &[obs(false, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: false });
        let a = g.step(ms(400), StepInput::default(), &[obs(false, "ref: main")]);
        assert_eq!(a, GateAction::Held { started: false });
        let a = g.step(ms(600), StepInput::default(), &[obs(false, "ref: main")]);
        assert!(
            matches!(a, GateAction::Flush { ref paths, .. } if paths == &[PathBuf::from("/w/a/late.rs")])
        );
    }

    /// A rebase stopped on a conflict: never wait forever.
    #[test]
    fn long_operation_is_flushed_after_max_hold() {
        let mut g = gate();
        let t0 = Instant::now();
        g.step(
            t0,
            StepInput {
                paths: paths(&["/w/a/x.rs"]),
                ..Default::default()
            },
            &[obs(true, "ref: main")],
        );
        let a = g.step(
            t0 + MAX_HOLD / 2,
            StepInput::default(),
            &[obs(true, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: false });
        let a = g.step(
            t0 + MAX_HOLD,
            StepInput::default(),
            &[obs(true, "ref: main")],
        );
        assert_eq!(
            a,
            GateAction::Flush {
                paths: paths(&["/w/a/x.rs"]),
                timed_out: true
            }
        );
        // Still busy: the next step opens a fresh hold.
        let a = g.step(
            t0 + MAX_HOLD + Duration::from_millis(50),
            StepInput::default(),
            &[obs(true, "ref: main")],
        );
        assert_eq!(a, GateAction::Held { started: true });
    }

    /// Two roots in two repositories: a busy one holds the other's paths too, and the flush
    /// only marks the directory whose refs actually moved.
    #[test]
    fn busy_repo_holds_every_root_and_marks_only_the_moved_one() {
        let mut g = GitGate::new(
            vec![dir("a"), dir("b")],
            vec![Some(b"m".to_vec()), Some(b"m".to_vec())],
            SETTLE,
        );
        let t0 = Instant::now();
        let a = g.step(
            t0,
            StepInput {
                paths: paths(&["/w/b/y.rs"]),
                ..Default::default()
            },
            &[obs(true, "m"), obs(false, "m")],
        );
        assert_eq!(a, GateAction::Held { started: true });
        let a = g.step(
            t0 + Duration::from_millis(10),
            StepInput::default(),
            &[obs(false, "n"), obs(false, "m")],
        );
        assert_eq!(a, GateAction::Held { started: false });
        let a = g.step(
            t0 + Duration::from_millis(10) + SETTLE,
            StepInput::default(),
            &[obs(false, "n"), obs(false, "m")],
        );
        assert_eq!(
            a,
            GateAction::Flush {
                paths: paths(&["/w/b/y.rs", "/w/a/.git/HEAD"]),
                timed_out: false
            }
        );
    }

    #[test]
    fn classify_git_event_by_location() {
        let d = GitDir {
            git_dir: PathBuf::from("/m/.git/worktrees/wt"),
            common_dir: PathBuf::from("/m/.git"),
            head_marker: PathBuf::from("/wt/.git/HEAD"),
        };
        let c = |p: &str| classify_git_event(&d, Path::new(p));
        assert_eq!(
            c("/m/.git/worktrees/wt/HEAD"),
            Some(GitEventKind::RefChange)
        );
        assert_eq!(
            c("/m/.git/worktrees/wt/index.lock"),
            Some(GitEventKind::Activity)
        );
        assert_eq!(
            c("/m/.git/worktrees/wt/rebase-merge/done"),
            Some(GitEventKind::Activity)
        );
        assert_eq!(c("/m/.git/refs/heads/topic"), Some(GitEventKind::RefChange));
        assert_eq!(c("/m/.git/packed-refs"), Some(GitEventKind::RefChange));
        assert_eq!(c("/m/.git/objects/ab/cdef"), Some(GitEventKind::Noise));
        assert_eq!(
            c("/m/.git/index.lock"),
            Some(GitEventKind::Noise),
            "main tree's lock"
        );
        assert_eq!(c("/wt/src/a.rs"), None);
    }

    #[test]
    fn young_lock_is_live_without_any_process_check() {
        let mut t = LockTracker::default();
        let now_wall = SystemTime::now();
        let calls = std::cell::Cell::new(0);
        let mut check = || {
            calls.set(calls.get() + 1);
            Some(false)
        };
        assert!(t.lock_is_live(
            Path::new("/g"),
            Some(now_wall - Duration::from_secs(5)),
            now_wall,
            Instant::now(),
            &mut check
        ));
        assert_eq!(calls.get(), 0, "no pgrep for a lock younger than 30s");
    }

    #[test]
    fn old_lock_without_git_process_is_orphaned_and_stays_ignored() {
        let mut t = LockTracker::default();
        let now_wall = SystemTime::now();
        let mtime = now_wall - Duration::from_secs(45);
        let now = Instant::now();
        let calls = std::cell::Cell::new(0);
        let mut check = || {
            calls.set(calls.get() + 1);
            Some(false)
        };
        assert!(!t.lock_is_live(Path::new("/g"), Some(mtime), now_wall, now, &mut check));
        assert!(!t.lock_is_live(Path::new("/g"), Some(mtime), now_wall, now, &mut check));
        assert_eq!(calls.get(), 1, "an orphan is declared once, not re-checked");
        // A *new* lock (different mtime) is live again.
        assert!(t.lock_is_live(Path::new("/g"), Some(now_wall), now_wall, now, &mut check));
    }

    #[test]
    fn old_lock_with_git_running_stays_live_and_recheck_is_rate_limited() {
        let mut t = LockTracker::default();
        let now_wall = SystemTime::now();
        let mtime = now_wall - Duration::from_secs(45);
        let now = Instant::now();
        let calls = std::cell::Cell::new(0);
        let mut check = || {
            calls.set(calls.get() + 1);
            Some(true)
        };
        assert!(t.lock_is_live(Path::new("/g"), Some(mtime), now_wall, now, &mut check));
        assert!(t.lock_is_live(
            Path::new("/g"),
            Some(mtime),
            now_wall,
            now + Duration::from_secs(1),
            &mut check
        ));
        assert_eq!(calls.get(), 1);
        assert!(t.lock_is_live(
            Path::new("/g"),
            Some(mtime),
            now_wall,
            now + PROCESS_RECHECK,
            &mut check
        ));
        assert_eq!(calls.get(), 2);
        // Unknown (check unavailable) never declares an orphan.
        let mut unknown = || None;
        let mut t = LockTracker::default();
        assert!(t.lock_is_live(Path::new("/g"), Some(mtime), now_wall, now, &mut unknown));
    }

    #[test]
    fn resolves_plain_repo_worktree_and_submodule_git_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canon");

        // Plain repository, root is a subdirectory of the work tree.
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git/refs/heads")).expect("mkdir");
        std::fs::create_dir_all(main.join("services/svc")).expect("mkdir");
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        let d = resolve_git_dir(&main.join("services/svc")).expect("plain");
        assert_eq!(d.git_dir, main.join(".git"));
        assert_eq!(d.common_dir, main.join(".git"));
        assert_eq!(d.head_marker, main.join(".git/HEAD"));

        // Linked worktree: `.git` file with an absolute `gitdir:` + `commondir`.
        let wt_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).expect("mkdir");
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/topic\n").expect("HEAD");
        std::fs::write(wt_git.join("commondir"), "../..\n").expect("commondir");
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).expect(".git");
        let d = resolve_git_dir(&wt).expect("worktree");
        assert_eq!(d.git_dir, wt_git);
        assert_eq!(d.common_dir, main.join(".git"));
        assert_eq!(d.head_marker, wt.join(".git/HEAD"));

        // Submodule: `.git` file with a *relative* pointer into the superproject.
        let sub_git = main.join(".git/modules/sub");
        std::fs::create_dir_all(&sub_git).expect("mkdir");
        std::fs::write(sub_git.join("HEAD"), "0123abcd\n").expect("HEAD");
        let sub = main.join("sub");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").expect(".git");
        let d = resolve_git_dir(&sub).expect("submodule");
        assert_eq!(d.git_dir, sub_git);
        assert_eq!(d.common_dir, sub_git);
        assert_eq!(d.head_marker, sub.join(".git/HEAD"));

        // Broken pointer → None, not a guess.
        let broken = base.join("broken");
        std::fs::create_dir_all(&broken).expect("mkdir");
        std::fs::write(broken.join(".git"), "gitdir: ./nowhere\n").expect(".git");
        assert_eq!(resolve_git_dir(&broken), None);
    }

    /// Same resolution against real `git worktree add` / `git submodule add` output, when a
    /// `git` binary is available (skipped otherwise — the synthetic test above still runs).
    #[test]
    fn resolves_real_git_worktree_and_submodule() {
        let git = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canon");
        let main = base.join("main");
        std::fs::create_dir_all(&main).expect("mkdir");
        if !git(&main, &["init", "-q"]) {
            eprintln!("git unavailable; skipping");
            return;
        }
        std::fs::write(main.join("a.txt"), "a").expect("write");
        assert!(git(&main, &["add", "."]));
        assert!(git(&main, &["commit", "-qm", "init"]));

        assert!(git(
            &main,
            &["worktree", "add", "-q", "../wt", "-b", "topic"]
        ));
        let wt = base.join("wt");
        let d = resolve_git_dir(&wt).expect("worktree");
        assert!(d.git_dir.starts_with(main.join(".git/worktrees")), "{d:?}");
        assert_eq!(d.common_dir, main.join(".git"));
        assert!(probe(&d)
            .head
            .is_some_and(|h| h.ends_with(b"refs/heads/topic\n")));

        let lib = base.join("lib");
        std::fs::create_dir_all(&lib).expect("mkdir");
        assert!(git(&lib, &["init", "-q"]));
        std::fs::write(lib.join("l.txt"), "l").expect("write");
        assert!(git(&lib, &["add", "."]));
        assert!(git(&lib, &["commit", "-qm", "lib"]));
        let lib_url = lib.to_string_lossy().to_string();
        assert!(git(&main, &["submodule", "add", "-q", &lib_url, "sub"]));
        let d = resolve_git_dir(&main.join("sub")).expect("submodule");
        assert_eq!(d.git_dir, main.join(".git/modules/sub"));
        assert_eq!(d.head_marker, main.join("sub/.git/HEAD"));
    }
}
