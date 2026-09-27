//! meshd — The MeshMCP Daemon
//!
//! A long-lived background process that:
//!   - Holds a single ContractGraph in memory (shared across all IDE sessions)
//!   - Runs exactly one FileWatcherService + one DifferentialVfs per machine
//!   - Multiplexes N client connections via Unix Domain Socket (UDS)
//!
//! IDE clients (`mesh-mcp run`) connect as lightweight proxies relaying stdio ↔ UDS.
//! On the first connection, mesh-mcp auto-spawns meshd if it is not already running.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod idle;
mod server;
mod socket;

use clap::Parser;
use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine};
use mesh_server::{FileWatcherService, WorkspaceIndexer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "meshd",
    version,
    about = "MeshMCP background daemon — shared graph + UDS multiplexer"
)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Minutes of inactivity before automatic shutdown (0 = never)
    #[arg(long, default_value = "30")]
    idle_timeout_minutes: u64,

    /// Seconds to wait for a first client to ever connect before shutting down as an orphaned
    /// daemon (0 = disabled). Always active by default (P2 step 3.3) since `ensure_daemon_running`
    /// auto-spawns `meshd` ad-hoc with no supervisor; set to 0 for manual/interactive debugging
    /// runs where you intend to attach a client more than 60s after starting this by hand.
    #[arg(long, default_value = "60")]
    startup_grace_secs: u64,

    /// Override the UDS socket path (Unix) or named pipe address (Windows)
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tracing to stderr only — stdout is reserved for MCP frames
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let args = Args::parse();

    // ── Config loading ────────────────────────────────────────────────────────
    // Resolved before the socket path: the default (no `--socket`) is scoped
    // to this workspace specifically (see below), which needs `base_dir` first.
    let (config, base_dir) = WorkspaceIndexer::discover_config(args.config.as_deref())?;
    let allowed_roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);

    // ── Socket path (Unix) / named pipe address (Windows) ───────────────────────
    // `meshd` has no UDS on Windows, so IDE clients share the daemon over a
    // named pipe instead. `--socket` (always passed by `mesh-mcp`'s own
    // auto-spawn) overrides either form; the default here — used only when
    // meshd is started by hand — is scoped to this workspace + binary
    // version, never the one-per-machine legacy path, so two unrelated
    // workspaces can never end up sharing (or racing to bind) the same
    // socket and silently serving each other's data (idempotence invariant
    // I7).
    #[cfg(unix)]
    let sock_path = match args.socket.clone() {
        Some(p) => p,
        None => socket::socket_path_for(&socket::workspace_id(&base_dir)),
    };

    // Clean up any stale socket from a previous crashed daemon
    #[cfg(unix)]
    socket::cleanup_stale_socket(&sock_path);

    #[cfg(windows)]
    let pipe_name = match args.socket.clone() {
        Some(p) => p.to_string_lossy().into_owned(),
        None => socket::pipe_name_for(&socket::workspace_id(&base_dir)),
    };

    // ── AppState (shared, single instance) ───────────────────────────────────
    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

    // ── Initial ingestion: runs in the background, not before the IPC server
    // starts. A large workspace's first scan can take long enough that a
    // client polling for the socket to appear (`mesh-mcp`'s `ensure_daemon_running`)
    // used to time out and silently fall back to standalone mode — spinning
    // up a second, redundant in-process index instead of just waiting a
    // little longer for the one meshd already building. The socket now
    // accepts connections immediately; `initialize`/`ping` succeed right
    // away, and `tools/call` reports "still indexing" (via `generation == 0`,
    // see `server::dispatch`) until this task's first `install_snapshot`.
    // Held under `reload_lock`, the same as every later `WorkspaceIndexer::reload`
    // call, so a filesystem event racing the initial scan can't install a
    // snapshot computed from a stale base out from under it (idempotence
    // invariant I2 — see `AppState::reload_lock`'s own doc).
    let ingest_state = state.clone();
    let ingest_base_dir = base_dir.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = ingest_state
            .reload_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tracing::info!(target: "meshd", "Starting initial workspace ingestion…");
        // Per-workspace cache under its quota (plan 4 step 4.4); a failure to open it degrades
        // to "parse everything" (see `AppState::open_index_cache`).
        let index_cache = ingest_state.open_index_cache(&ingest_base_dir);
        let snapshot = {
            let mut vfs = ingest_state.vfs.lock().unwrap_or_else(|e| e.into_inner());
            WorkspaceIndexer::build_snapshot(
                &ingest_state.config,
                &ingest_state.allowed_roots,
                None,
                Some(&mut vfs),
                index_cache,
            )
        };
        ingest_state.install_snapshot(snapshot);
        tracing::info!(
            target: "meshd",
            "Ingestion complete: {} contract nodes indexed.",
            ingest_state.snapshot().contract_graph.node_count()
        );
    });

    // ── Cancellation token + signal handler ───────────────────────────────────
    let cancel_token = CancellationToken::new();
    let cancel_sig = cancel_token.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "meshd", "SIGINT received. Initiating graceful shutdown.");
        cancel_sig.cancel();
    });

    // ── File watcher (single instance, shared across all clients) ─────────────
    // `FileWatcherService::spawn` does its (potentially slow, on a large workspace) setup —
    // the per-root directory walk and initial OS watch registration — synchronously before
    // returning, by design (see its own doc: an earlier version made that setup asynchronous to
    // avoid this exact blocking concern, which instead silently raced real filesystem events
    // against watch registration finishing). `spawn_blocking` moves that synchronous work off
    // this task's async worker thread instead, preserving the "socket accepts connections
    // immediately" invariant the ingestion block above already established, without touching
    // `FileWatcherService::spawn`'s own return-before-events-are-missed contract. Fire-and-forget
    // (not awaited), same as the ingestion task above — nothing here needs to block startup on it.
    let watcher_state = state.clone();
    let watcher_cancel = cancel_token.clone();
    tokio::task::spawn_blocking(move || {
        match FileWatcherService::spawn(watcher_state.clone(), watcher_cancel) {
            Ok(_handle) => {
                // Watches are confirmed live from this point on — but `spawn`'s own
                // (synchronous, possibly slow on macOS — see `MAX_WATCHED_DIRS`'s doc)
                // registration window means a file could have changed on disk before that
                // finished, with no watcher yet in place to observe it, and no other
                // mechanism to ever notice afterward (`reload`/`reload_paths` are purely
                // event-driven). One differential reload right here closes that gap: it
                // acquires `reload_lock` the same as the ingestion task above, so it safely
                // waits its turn if ingestion is still running, then does a cheap VFS diff
                // against whatever changed on disk since — including during setup above —
                // rather than leaving a possible silent divergence until the next restart.
                FileWatcherService::execute_reload_sync(&watcher_state);
            }
            Err(e) => {
                tracing::warn!(target: "meshd", "Failed to start FileWatcherService: {e}");
            }
        }
    });

    // ── Idle watchdog ─────────────────────────────────────────────────────────
    // Always spawned (P2 step 3.3), regardless of `idle_timeout_minutes`: the startup-grace
    // deadline below is a separate, unconditional-by-default guarantee against an orphaned
    // zombie daemon (`ensure_daemon_running` auto-spawns `meshd` ad-hoc, no launchd/systemd — if
    // that spawn races or a client never finds this daemon's socket at all, this is what
    // eventually cleans it up), not the opt-in "shut down once idle after real use" policy
    // `idle_timeout_minutes` controls. `idle_timeout_minutes == 0` disables *that* policy (an
    // effectively-infinite idle timeout below) without weakening the orphan guard.
    // `--startup-grace-secs 0` disables the orphan guard itself too — meant for a manual,
    // interactive `meshd` run where a developer intends to attach a client more than the default
    // 60s later, not for the ad-hoc auto-spawn path this guard exists for.
    let counter = idle::ClientCounter::new();
    let idle_timeout = if args.idle_timeout_minutes > 0 {
        Duration::from_secs(args.idle_timeout_minutes * 60)
    } else {
        Duration::from_secs(u64::MAX)
    };
    let startup_grace = if args.startup_grace_secs > 0 {
        Duration::from_secs(args.startup_grace_secs)
    } else {
        Duration::from_secs(u64::MAX)
    };
    idle::spawn_idle_watchdog(
        counter.clone(),
        cancel_token.clone(),
        idle_timeout,
        Duration::from_secs(10),
        startup_grace,
    );
    match (args.idle_timeout_minutes > 0, args.startup_grace_secs > 0) {
        (true, true) => tracing::info!(
            target: "meshd",
            "Idle watchdog active: shutdown after {} min with no clients (startup grace: {}s).",
            args.idle_timeout_minutes,
            args.startup_grace_secs
        ),
        (true, false) => tracing::info!(
            target: "meshd",
            "Idle watchdog active: shutdown after {} min with no clients (startup-grace orphan \
             guard disabled).",
            args.idle_timeout_minutes
        ),
        (false, true) => tracing::info!(
            target: "meshd",
            "Idle-after-use shutdown disabled; startup-grace orphan guard still active ({}s).",
            args.startup_grace_secs
        ),
        (false, false) => tracing::warn!(
            target: "meshd",
            "Both idle-after-use shutdown and the startup-grace orphan guard are disabled — this \
             daemon will run until explicitly stopped, even if no client ever connects."
        ),
    }

    // ── IPC server (blocking until cancelled) ─────────────────────────────────
    #[cfg(unix)]
    server::run_uds_server(&sock_path, state, cancel_token, counter)
        .await
        .map_err(|e| e.to_string())?;

    #[cfg(windows)]
    server::run_named_pipe_server(&pipe_name, state, cancel_token, counter)
        .await
        .map_err(|e| e.to_string())?;

    // Clean up socket on exit (Unix only — named pipes are released by the OS
    // once the last handle closes, there is no file to remove on Windows).
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&sock_path);
    }
    tracing::info!(target: "meshd", "meshd stopped. Socket removed.");

    Ok(())
}
