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
use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config, FilesystemCrawler,
    ValidatedScope,
};
use mesh_parsers::PolyglotIndexer;
use mesh_server::FileWatcherService;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "meshd",
    version = "2.9.0",
    about = "MeshMCP background daemon — shared graph + UDS multiplexer"
)]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Minutes of inactivity before automatic shutdown (0 = never)
    #[arg(long, default_value = "30")]
    idle_timeout_minutes: u64,

    /// Override the UDS socket path
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

    // ── Socket path ───────────────────────────────────────────────────────────
    let sock_path = match args.socket {
        Some(p) => p,
        None => socket::socket_path(),
    };

    // Clean up any stale socket from a previous crashed daemon
    socket::cleanup_stale_socket(&sock_path);

    // ── Config loading ────────────────────────────────────────────────────────
    let config_paths = [
        args.config.clone(),
        Some(PathBuf::from(".agents/mesh-mcp.toml")),
        Some(PathBuf::from("mesh-mcp.toml")),
    ];
    let found_path = config_paths.into_iter().flatten().find(|p| p.exists());

    let (config, base_dir) = if let Some(ref path) = found_path {
        let cfg = Config::load_from_file(path)?;
        let base = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        (cfg, base)
    } else {
        let default =
            "[workspace]\nname = \"default-mesh\"\nversion = \"2.9.0\"\nroots = [\".\"]\n";
        (Config::load_from_str(default)?, PathBuf::from("."))
    };

    let allowed_roots = match expand_roots(&config.workspace.roots, &base_dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "meshd", "Failed to expand roots: {e}. Falling back to cwd.");
            vec![dunce::canonicalize(&base_dir).unwrap_or(base_dir.clone())]
        }
    };

    // ── AppState (shared, single instance) ───────────────────────────────────
    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(
        config.clone(),
        allowed_roots.clone(),
        audit,
        rescan,
    ));

    // ── Initial ingestion ─────────────────────────────────────────────────────
    tracing::info!(target: "meshd", "Starting initial workspace ingestion…");
    for root in &allowed_roots {
        if let Ok(scope) = ValidatedScope::resolve(&root.to_string_lossy(), &allowed_roots) {
            let files = FilesystemCrawler::crawl_scope(
                &scope,
                &config.workspace.exclude_patterns,
                Some(10),
            );
            let mut graph = (*state.contract_graph.load().as_ref()).clone();
            let mut doc_index = (*state.doc_index.load().as_ref()).clone();
            let mut prop_reg = (*state.property_registry.load().as_ref()).clone();

            for file in files {
                if let Ok(content) = std::fs::read_to_string(&file) {
                    let path_str = file.to_string_lossy();
                    if path_str.ends_with(".md") {
                        doc_index.index_markdown_file(&file, &content);
                    } else if path_str.ends_with(".properties") {
                        prop_reg.ingest_properties_str(&content);
                    } else if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
                        let _ = prop_reg.ingest_yaml_str(&content);
                        PolyglotIndexer::index_file(&file, &content, 0, &mut graph);
                    } else {
                        PolyglotIndexer::index_file(&file, &content, 0, &mut graph);
                    }
                }
            }

            state.contract_graph.store(Arc::new(graph));
            state.doc_index.store(Arc::new(doc_index));
            state.property_registry.store(Arc::new(prop_reg));
        }
    }
    tracing::info!(
        target: "meshd",
        "Ingestion complete: {} contract nodes indexed.",
        state.contract_graph.load().node_count()
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

    // ── UDS server (blocking until cancelled) ─────────────────────────────────
    server::run_uds_server(&sock_path, state, cancel_token, counter)
        .await
        .map_err(|e| e.to_string())?;

    // Clean up socket on exit
    let _ = std::fs::remove_file(&sock_path);
    tracing::info!(target: "meshd", "meshd stopped. Socket removed.");

    Ok(())
}
