use clap::{Parser, Subcommand};
use mesh_core::{
    expand_roots, AppState, AuditLogger, BackgroundRescanEngine, Config, FilesystemCrawler,
    ValidatedScope,
};
use mesh_parsers::PolyglotIndexer;
use mesh_server::cli::{DoctorCommand, HooksCommand, InitCommand};
use mesh_server::run_server;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(
    name = "mesh-mcp",
    version = "2.9.0",
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
        /// Force standalone mode: skip daemon detection and run a full in-process server.
        /// Use this in containerised environments or when UDS is not available.
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
            HooksCommand::run()?;
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
                let sock_path = resolve_socket_path();
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

            #[cfg(not(unix))]
            let _ = standalone;

            // ── Standalone Mode (in-process fallback) ─────────────────────────
            run_standalone(cli.config.as_deref()).await?;
        }
    }

    Ok(())
}

// ── Proxy helpers ─────────────────────────────────────────────────────────────

/// Resolves the socket path — mirrors meshd/src/socket.rs logic.
#[cfg(unix)]
fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("MESH_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("mesh").join("meshd.sock");
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home)
            .join(".cache")
            .join("mesh")
            .join("meshd.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/mesh-{uid}.sock"))
}

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

// ── Standalone mode (full in-process server, original V2 behaviour) ───────────

async fn run_standalone(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let config_paths = [
        config_path.map(|p| p.to_path_buf()),
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

    let allowed_roots = match expand_roots(
        &config.workspace.roots,
        &base_dir,
        &config.workspace.workspace_root,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "mesh::config", "Failed to expand roots: {e}. Falling back to base directory.");
            vec![dunce::canonicalize(&base_dir).unwrap_or(base_dir.clone())]
        }
    };

    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(
        config.clone(),
        allowed_roots.clone(),
        audit,
        rescan,
    ));

    for (repo_idx, root) in allowed_roots.iter().enumerate() {
        let repo_id = repo_idx as mesh_core::RepoId;
        if let Ok(validated_scope) =
            ValidatedScope::resolve(&root.to_string_lossy(), &allowed_roots)
        {
            let files = FilesystemCrawler::crawl_scope(
                &validated_scope,
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
                        PolyglotIndexer::index_file(&file, &content, repo_id, &mut graph);
                    } else {
                        PolyglotIndexer::index_file(&file, &content, repo_id, &mut graph);
                    }

                    if let Some(ref contracts_cfg) = state.config.load().engines.contracts {
                        PolyglotIndexer::apply_custom_patterns(
                            &file,
                            &content,
                            repo_id,
                            &contracts_cfg.patterns,
                            &mut graph,
                        );
                    }
                }
            }

            graph.reconcile_edges();
            state.contract_graph.store(Arc::new(graph));
            state.doc_index.store(Arc::new(doc_index));
            state.property_registry.store(Arc::new(prop_reg));
        }
    }

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
