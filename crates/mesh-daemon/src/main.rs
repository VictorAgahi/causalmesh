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

    // ── Socket path (Unix) / named pipe address (Windows) ───────────────────────
    // `meshd` has no UDS on Windows, so IDE clients share the daemon over a
    // named pipe instead. `--socket` overrides either form.
    #[cfg(unix)]
    let sock_path = match args.socket.clone() {
        Some(p) => p,
        None => socket::socket_path(),
    };

    // Clean up any stale socket from a previous crashed daemon
    #[cfg(unix)]
    socket::cleanup_stale_socket(&sock_path);

    #[cfg(windows)]
    let pipe_name = match args.socket.clone() {
        Some(p) => p.to_string_lossy().into_owned(),
        None => socket::pipe_name(),
    };

    // ── Config loading ────────────────────────────────────────────────────────
    let (config, base_dir) = WorkspaceIndexer::discover_config(args.config.as_deref())?;
    let allowed_roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);

    // ── AppState (shared, single instance) ───────────────────────────────────
    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

    // ── Initial ingestion: one parallel scan, one reconcile, one atomic install ─
    tracing::info!(target: "meshd", "Starting initial workspace ingestion…");
    let snapshot = {
        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());
        WorkspaceIndexer::build_snapshot(&state.config, &state.allowed_roots, None, Some(&mut vfs))
    };
    state.install_snapshot(snapshot);
    tracing::info!(
        target: "meshd",
        "Ingestion complete: {} contract nodes indexed.",
        state.snapshot().contract_graph.node_count()
    );

    // ── Cancellation token + signal handler ───────────────────────────────────
    let cancel_token = CancellationToken::new();
    let cancel_sig = cancel_token.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "meshd", "SIGINT received. Initiating graceful shutdown.");
        cancel_sig.cancel();
    });

    // ── File watcher (single instance, shared across all clients) ─────────────
    if let Err(e) = FileWatcherService::spawn(state.clone(), cancel_token.clone()) {
        tracing::warn!(target: "meshd", "Failed to start FileWatcherService: {e}");
    }

    // ── Idle watchdog ─────────────────────────────────────────────────────────
    let counter = idle::ClientCounter::new();
    if args.idle_timeout_minutes > 0 {
        idle::spawn_idle_watchdog(
            counter.clone(),
            cancel_token.clone(),
            Duration::from_secs(args.idle_timeout_minutes * 60),
            Duration::from_secs(10),
        );
        tracing::info!(
            target: "meshd",
            "Idle watchdog active: shutdown after {} min with no clients.",
            args.idle_timeout_minutes
        );
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
