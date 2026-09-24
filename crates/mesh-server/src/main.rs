use clap::{Parser, Subcommand};
use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine};
use mesh_server::cli::{DoctorCommand, HooksCommand, InitCommand, StatsCommand};
use mesh_server::{run_server, WorkspaceIndexer};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "mesh-mcp",
    version,
    about = "Universal Polyglot Architecture Mesh & Contract Governance MCP Server"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long, global = true, help = "Path to custom configuration file")]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the MeshMCP JSON-RPC server over stdio.
    /// By default, connects to the shared meshd daemon via UDS for zero-overhead operation.
    /// Falls back to --standalone mode if daemon is unavailable.
    Run {
        #[arg(long, default_value = "false")]
        standalone: bool,
    },

    /// Run diagnostic healthchecks on environment, permissions, and roots
    Doctor,

    /// Automatically scan polyglot workspace and generate .agents/mesh-mcp.toml
    Init {
        #[arg(
            long,
            help = "Automatically detect all services and schemas without prompts"
        )]
        auto: bool,
        #[arg(
            long,
            help = "Generate IDE configurations for Cursor, VS Code, and Claude Code"
        )]
        write_ide_config: bool,
    },

    /// Install OS-level Git pre-commit hooks for active governance
    InstallHooks,

    /// Summarize local audit-log usage: calls per tool, error rate, and
    /// most-queried scopes/targets. Reads `audit.db` read-only; nothing leaves
    /// the machine.
    Stats {
        /// Time window to include: `<N>s`, `<N>m`, `<N>h`, `<N>d`, or `all`.
        #[arg(long, default_value = "7d")]
        since: String,
    },

    /// Generate and view an interactive architecture graph of services, contracts, and topics
    Graph {
        /// Format of the output: html, mermaid, or json
        #[arg(short, long, default_value = "html")]
        format: String,

        /// Optional file path to write the output to
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Open the resulting visualization directly in your default browser
        #[arg(long, default_value = "false")]
        open: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Commandment 3: Route all tracing strictly to stderr so stdout is reserved for JSON-RPC MCP frames
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Commands::Run { standalone: false }) {
        Commands::Doctor => {
            DoctorCommand::run(cli.config.as_deref())?;
        }
        Commands::Init {
            auto,
            write_ide_config,
        } => {
            InitCommand::run(auto, write_ide_config)?;
        }
        Commands::InstallHooks => {
            let (config, _base) = WorkspaceIndexer::discover_config(cli.config.as_deref())?;
            if HooksCommand::is_enabled(&config) {
                HooksCommand::run(&config)?;
            } else {
                eprintln!(
                    "✖ Skipped: [engines.policy] enforce_git_hooks = false — hook installation disabled by config."
                );
            }
        }
        Commands::Stats { since } => {
            StatsCommand::run(None, Some(&since))?;
        }
        Commands::Graph {
            format,
            output,
            open,
        } => {
            mesh_server::cli::GraphCommand::run(
                cli.config.as_deref(),
                &format,
                output.as_deref(),
                open,
            )?;
        }
        Commands::Run { standalone } => {
            #[cfg(unix)]
            if !standalone {
                // ── UDS Proxy Mode ────────────────────────────────────────────
                let sock_path = mesh_core::socket_path();
                match ensure_daemon_running(&sock_path).await {
                    Ok(()) => {
                        tracing::info!(
                            target: "mesh::proxy",
                            "Connecting to meshd at {}",
                            sock_path.display()
                        );
                        return run_proxy_mode(&sock_path).await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "mesh::proxy",
                            "Could not connect to meshd ({}). Falling back to standalone mode.",
                            e
                        );
                    }
                }
            }

            #[cfg(windows)]
            if !standalone {
                // ── Named Pipe Proxy Mode ──────────────────────────────────────
                let pipe_name = mesh_core::pipe_name();
                match ensure_daemon_running_windows(&pipe_name).await {
                    Ok(()) => {
                        tracing::info!(
                            target: "mesh::proxy",
                            "Connecting to meshd at {}",
                            pipe_name
                        );
                        return run_proxy_mode_windows(&pipe_name).await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "mesh::proxy",
                            "Could not connect to meshd ({}). Falling back to standalone mode.",
                            e
                        );
                    }
                }
            }

            #[cfg(not(any(unix, windows)))]
            let _ = standalone;

            // ── Standalone Mode (in-process fallback) ─────────────────────────
            run_standalone(cli.config.as_deref()).await?;
        }
    }

    Ok(())
}

// ── Proxy helpers ─────────────────────────────────────────────────────────────

/// Checks if meshd is alive. If not, auto-spawns it and waits up to 500ms.
#[cfg(unix)]
async fn ensure_daemon_running(
    sock_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if tokio::net::UnixStream::connect(sock_path).await.is_ok() {
        return Ok(());
    }

    tracing::info!(target: "mesh::proxy", "meshd not found. Attempting auto-spawn…");

    let meshd_path = std::env::current_exe()?
        .parent()
        .ok_or("Cannot determine exe dir")?
        .join("meshd");

    if !meshd_path.exists() {
        return Err(format!("meshd binary not found at {}", meshd_path.display()).into());
    }

    std::process::Command::new(&meshd_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn meshd: {e}"))?;

    // Poll up to 500ms for socket to appear
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if tokio::net::UnixStream::connect(sock_path).await.is_ok() {
            tracing::info!(target: "mesh::proxy", "meshd started successfully.");
            return Ok(());
        }
    }

    Err("meshd did not bind socket within 500ms".into())
}

/// Ultra-lightweight proxy: bridges stdin/stdout ↔ UDS (zero-copy, < 2 MiB footprint).
#[cfg(unix)]
async fn run_proxy_mode(sock_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;

    let stream = tokio::net::UnixStream::connect(sock_path).await?;
    let (daemon_reader, mut daemon_writer) = stream.into_split();

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let stdin_to_daemon = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdin, &mut daemon_writer).await;
        let _ = daemon_writer.shutdown().await;
    });

    let daemon_to_stdout = tokio::spawn(async move {
        let mut daemon_reader = daemon_reader;
        let _ = tokio::io::copy(&mut daemon_reader, &mut stdout).await;
        let _ = stdout.flush().await;
    });

    tokio::pin!(daemon_to_stdout);

    // If daemon disconnects, terminate immediately.
    // If stdin closes (e.g. echo pipe in CI), wait for daemon to drain response.
    tokio::select! {
        _ = stdin_to_daemon => {
            let _ = (&mut daemon_to_stdout).await;
        }
        _ = &mut daemon_to_stdout => {}
    }

    Ok(())
}

// ── Windows named pipe proxy helpers ─────────────────────────────────────────
//
// meshd has no Unix Domain Socket on Windows, so `mesh-mcp run` shares the
// daemon over a named pipe instead, mirroring the UDS proxy helpers above.

/// Checks if meshd is alive. If not, auto-spawns it and waits up to 500ms.
#[cfg(windows)]
async fn ensure_daemon_running_windows(
    pipe_name: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    if ClientOptions::new().open(pipe_name).is_ok() {
        return Ok(());
    }

    tracing::info!(target: "mesh::proxy", "meshd not found. Attempting auto-spawn…");

    let meshd_path = std::env::current_exe()?
        .parent()
        .ok_or("Cannot determine exe dir")?
        .join("meshd.exe");

    if !meshd_path.exists() {
        return Err(format!("meshd binary not found at {}", meshd_path.display()).into());
    }

    std::process::Command::new(&meshd_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn meshd: {e}"))?;

    // Poll up to 500ms for the pipe to appear
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if ClientOptions::new().open(pipe_name).is_ok() {
            tracing::info!(target: "mesh::proxy", "meshd started successfully.");
            return Ok(());
        }
    }

    Err("meshd did not bind its named pipe within 500ms".into())
}

/// Ultra-lightweight proxy: bridges stdin/stdout ↔ named pipe.
#[cfg(windows)]
async fn run_proxy_mode_windows(pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new().open(pipe_name)?;
    let (daemon_reader, mut daemon_writer) = tokio::io::split(client);

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let stdin_to_daemon = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdin, &mut daemon_writer).await;
        let _ = daemon_writer.shutdown().await;
    });

    let daemon_to_stdout = tokio::spawn(async move {
        let mut daemon_reader = daemon_reader;
        let _ = tokio::io::copy(&mut daemon_reader, &mut stdout).await;
        let _ = stdout.flush().await;
    });

    tokio::pin!(daemon_to_stdout);

    // If daemon disconnects, terminate immediately.
    // If stdin closes (e.g. echo pipe in CI), wait for daemon to drain response.
    tokio::select! {
        _ = stdin_to_daemon => {
            let _ = (&mut daemon_to_stdout).await;
        }
        _ = &mut daemon_to_stdout => {}
    }

    Ok(())
}

// ── Standalone mode (full in-process server, original V2 behaviour) ───────────

async fn run_standalone(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let (config, base_dir) = WorkspaceIndexer::discover_config(config_path)?;
    let allowed_roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);

    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

    // Single parallel scan over all roots, one reconcile, one atomic install.
    let snapshot = {
        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());
        WorkspaceIndexer::build_snapshot(&state.config, &state.allowed_roots, None, Some(&mut vfs))
    };
    state.install_snapshot(snapshot);

    let cancel_token = CancellationToken::new();
    let cancel_sig = cancel_token.clone();

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "mesh::shutdown", "SIGINT/Ctrl-C received. Initiating graceful shutdown.");
        cancel_sig.cancel();
    });

    if let Err(e) = mesh_server::FileWatcherService::spawn(state.clone(), cancel_token.clone()) {
        tracing::warn!(target: "mesh::watcher", "Failed to start FileWatcherService: {e}");
    }

    run_server(state, cancel_token).await?;
    Ok(())
}
