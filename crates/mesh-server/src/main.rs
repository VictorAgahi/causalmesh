#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

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
    Doctor {
        /// Repair what can safely be repaired: socket permissions, a stale
        /// daemon left over from before an upgrade (stopped, never another
        /// process), a corrupt or pre-4.4 legacy cache, an orphaned
        /// per-version workspace cache directory. Never touches the audit
        /// trail — its corruption is reported, not erased.
        #[arg(long, default_value = "false")]
        fix: bool,

        /// Emit the repairable-health checks (section 10) as JSON on stdout,
        /// for install scripts. Sections 1-9's prose still goes to stderr.
        #[arg(long, default_value = "false")]
        json: bool,
    },

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
        /// Format of the output: html, mermaid, json, or fingerprint (content hash of the
        /// full index, for comparing two runs)
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
        Commands::Doctor { fix, json } => {
            DoctorCommand::run(cli.config.as_deref(), fix, json)?;
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
            // Discovered once, up front, for every mode below: the workspace
            // this session concerns determines which daemon it may talk to
            // (idempotence invariant I7 — a response always concerns the
            // workspace of the session that asked). Falling back to
            // standalone on a discovery error would silently index the wrong
            // (default, cwd-relative) workspace instead.
            let (_, base_dir) = WorkspaceIndexer::discover_config(cli.config.as_deref())?;
            let canonical_base = dunce::canonicalize(&base_dir).unwrap_or(base_dir);
            let workspace_id = mesh_core::workspace_id(&canonical_base);

            #[cfg(unix)]
            if !standalone {
                // ── UDS Proxy Mode ────────────────────────────────────────────
                let sock_path = mesh_core::socket_path_for(&workspace_id);
                let ensured =
                    ensure_daemon_running(&sock_path, &canonical_base, cli.config.as_deref()).await;
                if try_proxy_unix(ensured, &sock_path, &workspace_id).await? == ProxyAttempt::Served
                {
                    return Ok(());
                }
            }

            #[cfg(windows)]
            if !standalone {
                // ── Named Pipe Proxy Mode ──────────────────────────────────────
                let pipe_name = mesh_core::pipe_name_for(&workspace_id);
                let ensured = ensure_daemon_running_windows(
                    &pipe_name,
                    &canonical_base,
                    cli.config.as_deref(),
                )
                .await;
                if try_proxy_windows(ensured, &pipe_name, &workspace_id).await?
                    == ProxyAttempt::Served
                {
                    return Ok(());
                }
            }

            #[cfg(not(any(unix, windows)))]
            let _ = (standalone, workspace_id);

            // ── Standalone Mode (in-process fallback) ─────────────────────────
            run_standalone(cli.config.as_deref()).await?;
        }
    }

    Ok(())
}

/// Compares an already-running daemon's recorded version (plan 4 step 4.7:
/// written by `meshd` itself at startup, `mesh_core::socket::write_daemon_meta`)
/// against this `mesh-mcp` binary's own, and warns once if they differ — a
/// daemon left over from before an upgrade otherwise keeps serving an older
/// snapshot format or behavior with no visible sign why. Silent (not an
/// error) when no metadata is on record: an older daemon that predates this
/// check, or one whose write failed, is not itself a fault.
fn warn_on_daemon_version_mismatch(workspace_id: &str) {
    if let Some(meta) = mesh_core::socket::read_daemon_meta(workspace_id) {
        if meta.version != env!("CARGO_PKG_VERSION") {
            tracing::warn!(
                target: "mesh::proxy",
                "meshd (pid {}) is running version {}, but this mesh-mcp is {} — \
                 restart it (`mesh-mcp doctor --fix` stops the mismatched daemon) \
                 to pick up the newer version.",
                meta.pid,
                meta.version,
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

// ── Proxy helpers ─────────────────────────────────────────────────────────────

/// Rotates and opens this workspace's `meshd` auto-spawn log (P2 step 3.3), replacing the old
/// `Stdio::null()` — a daemon spawned ad-hoc by `ensure_daemon_running`/
/// `ensure_daemon_running_windows` has no supervisor capturing its output, so a crash or panic
/// *before* its own `tracing` subscriber initializes (or one bypassing it entirely, e.g. a
/// Rust panic's default handler, which writes to stderr directly) was previously unobservable.
/// One file per workspace (named by `workspace_id`, the same scoping `socket_path_for` already
/// uses, so concurrent daemons for different workspaces never interleave into the same file);
/// up to `MAX_ROTATIONS` previous runs are kept (`meshd-<id>.log.1` most recent,
/// `.MAX_ROTATIONS` oldest) so a repeatedly-crashing daemon's history survives past the very
/// next restart without growing the log directory unboundedly. Shared, platform-independent
/// code — not `#[cfg(unix)]`-gated, since both the Unix and Windows auto-spawn paths use it.
const DAEMON_LOG_MAX_ROTATIONS: usize = 5;

fn open_daemon_log(workspace_id: &str) -> std::io::Result<std::fs::File> {
    let log_dir = mesh_core::mesh_cache_dir().join("logs");
    rotate_and_open_log(&log_dir, workspace_id, DAEMON_LOG_MAX_ROTATIONS)
}

/// The rotation algorithm itself, factored out of [`open_daemon_log`] so it can be unit-tested
/// against a temp directory instead of the real `~/.cache/mesh-mcp/logs` (which every other
/// `~/.cache/mesh-mcp/*` helper in this codebase — `AuditLogger`, `PersistentIndexCache` —
/// likewise never unit-tests directly, for the same reason: it's process-wide, shared, real
/// user state, not something a test should create, rotate or delete).
fn rotate_and_open_log(
    log_dir: &Path,
    workspace_id: &str,
    max_rotations: usize,
) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(log_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(log_dir, std::fs::Permissions::from_mode(0o700));
    }

    let current = log_dir.join(format!("meshd-{workspace_id}.log"));
    let oldest = log_dir.join(format!("meshd-{workspace_id}.log.{max_rotations}"));
    if oldest.exists() {
        if let Err(e) = std::fs::remove_file(&oldest) {
            tracing::warn!(target: "mesh::proxy", "Failed to remove oldest rotated daemon log {}: {e}", oldest.display());
        }
    }
    for i in (1..max_rotations).rev() {
        let from = log_dir.join(format!("meshd-{workspace_id}.log.{i}"));
        if !from.exists() {
            continue;
        }
        let to = log_dir.join(format!("meshd-{workspace_id}.log.{}", i + 1));
        if let Err(e) = std::fs::rename(&from, &to) {
            tracing::warn!(target: "mesh::proxy", "Failed to rotate daemon log {} -> {}: {e}", from.display(), to.display());
        }
    }
    if current.exists() {
        let rotated = log_dir.join(format!("meshd-{workspace_id}.log.1"));
        if let Err(e) = std::fs::rename(&current, &rotated) {
            tracing::warn!(target: "mesh::proxy", "Failed to rotate current daemon log to {}: {e}", rotated.display());
        }
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&current)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

/// `stdout`/`stderr` `Stdio` for a freshly `open_daemon_log`-ed file, sharing one file
/// description (via `try_clone`) so writes from both streams land in one consistent, ordered
/// file rather than two independently-buffered views of the same path. Falls back to
/// `Stdio::null()` (the pre-3.3 behaviour) if the log can't be opened at all — a daemon that
/// can't be auto-spawned with logging is still better than one that can't be spawned.
fn daemon_output_stdio(workspace_id: &str) -> (std::process::Stdio, std::process::Stdio) {
    match open_daemon_log(workspace_id) {
        Ok(file) => match file.try_clone() {
            Ok(file2) => (
                std::process::Stdio::from(file),
                std::process::Stdio::from(file2),
            ),
            Err(e) => {
                tracing::warn!(target: "mesh::proxy", "Failed to duplicate meshd log handle: {e}");
                (std::process::Stdio::from(file), std::process::Stdio::null())
            }
        },
        Err(e) => {
            tracing::warn!(target: "mesh::proxy", "Failed to open meshd log file, discarding daemon output: {e}");
            (std::process::Stdio::null(), std::process::Stdio::null())
        }
    }
}

/// Checks if this workspace's meshd is alive at `sock_path`. If not, spawns
/// it — with `--socket sock_path` and `.current_dir(base_dir)` (plus
/// `--config` when the caller passed an explicit one) so the daemon binds
/// exactly the socket this proxy is about to connect to and indexes exactly
/// this workspace, never whichever config its own cwd-based discovery might
/// otherwise land on — then waits up to 500ms for it to bind.
///
/// That 500ms budget used to race a real risk: `meshd` indexed its whole
/// workspace *before* opening its socket, so on a large repo this would
/// reliably time out and fall back to standalone mode — spinning up a
/// second, redundant in-process index right as the daemon it gave up on
/// finished its own. Since P0 step 1.8, `meshd` opens its socket first and
/// ingests in the background, so binding it back is now independent of
/// workspace size and this budget is comfortably generous rather than a race.
#[cfg(unix)]
async fn ensure_daemon_running(
    sock_path: &Path,
    base_dir: &Path,
    explicit_config: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if tokio::net::UnixStream::connect(sock_path).await.is_ok() {
        warn_on_daemon_version_mismatch(&mesh_core::workspace_id(base_dir));
        return Ok(());
    }

    tracing::info!(target: "mesh::proxy", "meshd not found for this workspace. Attempting auto-spawn…");

    let meshd_path = std::env::current_exe()?
        .parent()
        .ok_or("Cannot determine exe dir")?
        .join("meshd");

    if !meshd_path.exists() {
        return Err(format!("meshd binary not found at {}", meshd_path.display()).into());
    }

    let (stdout_io, stderr_io) = daemon_output_stdio(&mesh_core::workspace_id(base_dir));
    let mut cmd = std::process::Command::new(&meshd_path);
    cmd.current_dir(base_dir)
        .arg("--socket")
        .arg(sock_path)
        .stdin(std::process::Stdio::null())
        .stdout(stdout_io)
        .stderr(stderr_io);
    if let Some(config) = explicit_config {
        let canonical_config = dunce::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
        cmd.arg("--config").arg(canonical_config);
    }
    cmd.spawn()
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

/// Whether this session was served through `meshd` or must fall back to the
/// in-process standalone server.
#[cfg(any(unix, windows))]
#[derive(Debug, PartialEq, Eq)]
enum ProxyAttempt {
    Served,
    Fallback,
}

/// How a proxy session ended.
#[cfg(any(unix, windows))]
#[derive(Debug, PartialEq, Eq)]
enum ProxyEnd {
    /// The MCP client closed stdin; the daemon's last responses were drained.
    ClientClosed,
    /// The daemon side closed or broke first: `meshd` exited or crashed.
    DaemonClosed,
}

/// Logged on stderr when the daemon dies mid-session (plan 4 step 4.13 (b),
/// option (i)): the zero-copy proxy has no framing, so it cannot answer the
/// requests in flight — it closes stdout, and this says why.
#[cfg(any(unix, windows))]
const DAEMON_LOST_MESSAGE: &str = "meshd closed the connection mid-session";

/// Logged on stderr when the proxy's own connection fails right after
/// `ensure_daemon_running` succeeded (plan 4 step 4.13 (a)).
#[cfg(any(unix, windows))]
const POST_ENSURE_FALLBACK_MESSAGE: &str =
    "meshd answered its liveness check but the proxy connection failed";

/// Ends a proxy session. A daemon that went away first is logged as an error
/// with a readable cause, then the process exits with status 1: tokio reads
/// stdin on a blocking thread that the runtime waits for on shutdown, so merely
/// returning left the process alive with stdout open while the client still
/// held stdin — a hang instead of an EOF (plan 4 step 4.13 (b), option (i)).
#[cfg(any(unix, windows))]
fn finish_proxy_session(end: &ProxyEnd, endpoint: &str, workspace_id: &str) {
    if *end == ProxyEnd::DaemonClosed {
        let log = mesh_core::mesh_cache_dir()
            .join("logs")
            .join(format!("meshd-{workspace_id}.log"));
        tracing::error!(
            target: "mesh::proxy",
            "{DAEMON_LOST_MESSAGE} ({endpoint}): the daemon exited or crashed. \
             Requests in flight get no response and this MCP session ends (EOF on stdout); \
             restart the MCP server. Daemon log: {}",
            log.display()
        );
        std::process::exit(1);
    }
}

/// Serves this session through `meshd` at `sock_path` once `ensure_daemon_running`
/// returned `ensured`. Either failure — the daemon could not be reached or
/// spawned, or it answered the liveness check but the proxy's own connection
/// then failed (a daemon exiting in between) — falls back to standalone
/// instead of closing stdout on the client.
#[cfg(unix)]
async fn try_proxy_unix(
    ensured: Result<(), Box<dyn std::error::Error + Send + Sync>>,
    sock_path: &Path,
    workspace_id: &str,
) -> Result<ProxyAttempt, Box<dyn std::error::Error>> {
    if let Err(e) = ensured {
        tracing::warn!(
            target: "mesh::proxy",
            "Could not connect to meshd ({}). Falling back to standalone mode.",
            e
        );
        return Ok(ProxyAttempt::Fallback);
    }
    let stream = match tokio::net::UnixStream::connect(sock_path).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::warn!(
                target: "mesh::proxy",
                "{POST_ENSURE_FALLBACK_MESSAGE} ({}: {e}). Falling back to standalone mode.",
                sock_path.display()
            );
            return Ok(ProxyAttempt::Fallback);
        }
    };
    tracing::info!(
        target: "mesh::proxy",
        "Connecting to meshd at {} (workspace {})",
        sock_path.display(),
        workspace_id
    );
    let (daemon_reader, daemon_writer) = stream.into_split();
    let end = bridge(
        daemon_reader,
        daemon_writer,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await;
    finish_proxy_session(&end, &sock_path.display().to_string(), workspace_id);
    Ok(ProxyAttempt::Served)
}

/// Ultra-lightweight proxy: bridges the client's stdin/stdout and the daemon's
/// stream (zero-copy, < 2 MiB footprint). If the daemon disconnects, it ends
/// at once; if stdin closes (e.g. an `echo` pipe in CI), it waits for the
/// daemon to drain its responses.
#[cfg(any(unix, windows))]
async fn bridge<DR, DW, I, O>(
    mut daemon_reader: DR,
    mut daemon_writer: DW,
    mut stdin: I,
    mut stdout: O,
) -> ProxyEnd
where
    DR: tokio::io::AsyncRead + Unpin + Send + 'static,
    DW: tokio::io::AsyncWrite + Unpin + Send + 'static,
    I: tokio::io::AsyncRead + Unpin + Send + 'static,
    O: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;

    // Set when stdin reached EOF, *before* the daemon is told (shutdown): once
    // the daemon closes in answer to that, the end is the client's, not a
    // daemon failure — whichever task the select below happens to see first.
    let client_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client_done_writer = Arc::clone(&client_done);
    let stdin_to_daemon = tokio::spawn(async move {
        if tokio::io::copy(&mut stdin, &mut daemon_writer)
            .await
            .is_ok()
        {
            client_done_writer.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let _ = daemon_writer.shutdown().await;
    });

    let daemon_to_stdout = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut daemon_reader, &mut stdout).await;
        let _ = stdout.flush().await;
    });

    tokio::pin!(daemon_to_stdout);

    tokio::select! {
        _ = stdin_to_daemon => {
            let _ = (&mut daemon_to_stdout).await;
        }
        _ = &mut daemon_to_stdout => {}
    }
    if client_done.load(std::sync::atomic::Ordering::SeqCst) {
        ProxyEnd::ClientClosed
    } else {
        ProxyEnd::DaemonClosed
    }
}

// ── Windows named pipe proxy helpers ─────────────────────────────────────────
//
// meshd has no Unix Domain Socket on Windows, so `mesh-mcp run` shares the
// daemon over a named pipe instead, mirroring the UDS proxy helpers above.

/// Checks if this workspace's meshd is alive at `pipe_name`. If not, spawns
/// it scoped to this workspace — mirroring the Unix `ensure_daemon_running`
/// above (see its doc for why `--socket`/`.current_dir` matter and why the
/// 500ms budget is no longer a race since P0 step 1.8).
#[cfg(windows)]
async fn ensure_daemon_running_windows(
    pipe_name: &str,
    base_dir: &Path,
    explicit_config: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    if ClientOptions::new().open(pipe_name).is_ok() {
        warn_on_daemon_version_mismatch(&mesh_core::workspace_id(base_dir));
        return Ok(());
    }

    tracing::info!(target: "mesh::proxy", "meshd not found for this workspace. Attempting auto-spawn…");

    let meshd_path = std::env::current_exe()?
        .parent()
        .ok_or("Cannot determine exe dir")?
        .join("meshd.exe");

    if !meshd_path.exists() {
        return Err(format!("meshd binary not found at {}", meshd_path.display()).into());
    }

    let (stdout_io, stderr_io) = daemon_output_stdio(&mesh_core::workspace_id(base_dir));
    let mut cmd = std::process::Command::new(&meshd_path);
    cmd.current_dir(base_dir)
        .arg("--socket")
        .arg(pipe_name)
        .stdin(std::process::Stdio::null())
        .stdout(stdout_io)
        .stderr(stderr_io);
    if let Some(config) = explicit_config {
        let canonical_config = dunce::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
        cmd.arg("--config").arg(canonical_config);
    }
    cmd.spawn()
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

/// Windows mirror of [`try_proxy_unix`] over this workspace's named pipe.
#[cfg(windows)]
async fn try_proxy_windows(
    ensured: Result<(), Box<dyn std::error::Error + Send + Sync>>,
    pipe_name: &str,
    workspace_id: &str,
) -> Result<ProxyAttempt, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    if let Err(e) = ensured {
        tracing::warn!(
            target: "mesh::proxy",
            "Could not connect to meshd ({}). Falling back to standalone mode.",
            e
        );
        return Ok(ProxyAttempt::Fallback);
    }
    let client = match ClientOptions::new().open(pipe_name) {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(
                target: "mesh::proxy",
                "{POST_ENSURE_FALLBACK_MESSAGE} ({pipe_name}: {e}). Falling back to standalone mode."
            );
            return Ok(ProxyAttempt::Fallback);
        }
    };
    tracing::info!(
        target: "mesh::proxy",
        "Connecting to meshd at {} (workspace {})",
        pipe_name,
        workspace_id
    );
    let (daemon_reader, daemon_writer) = tokio::io::split(client);
    let end = bridge(
        daemon_reader,
        daemon_writer,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await;
    finish_proxy_session(&end, pipe_name, workspace_id);
    Ok(ProxyAttempt::Served)
}

// ── Standalone mode (full in-process server, original V2 behaviour) ───────────

async fn run_standalone(config_path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let (config, base_dir) = WorkspaceIndexer::discover_config(config_path)?;
    let allowed_roots = WorkspaceIndexer::resolve_roots(&config, &base_dir);

    let audit = Arc::new(AuditLogger::new(None)?);
    let rescan = Arc::new(BackgroundRescanEngine::new()?);
    let state = Arc::new(AppState::new(config, allowed_roots, audit, rescan));

    // Persistent, content-hash-keyed cache of tree-sitter `FileIndex` fragments (P2 step 3.2),
    // one database per workspace under its quota (plan 4 step 4.4); see
    // `AppState::open_index_cache` for why a failure to open it does not fail the boot.
    let index_cache = state.open_index_cache(&base_dir);

    // Single parallel scan over all roots, one reconcile, one atomic install.
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

    let cancel_token = CancellationToken::new();
    let cancel_sig = cancel_token.clone();

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!(target: "mesh::shutdown", "SIGINT/Ctrl-C received. Initiating graceful shutdown.");
        cancel_sig.cancel();
    });

    // `spawn_blocking`, not called inline: `FileWatcherService::spawn`'s setup (the per-root
    // directory walk and initial OS watch registration) runs synchronously before returning —
    // deliberately, so watches are guaranteed live the instant it returns (see its own doc). On
    // macOS that setup can take real time (`MAX_WATCHED_DIRS`'s doc), so running it on this
    // process's single async task would freeze the whole stdio/MCP proxy during startup instead
    // of just delaying watcher readiness, exactly like `meshd`'s equivalent call.
    let watcher_state = state.clone();
    let watcher_cancel = cancel_token.clone();
    tokio::task::spawn_blocking(move || {
        match mesh_server::FileWatcherService::spawn(watcher_state.clone(), watcher_cancel) {
            Ok(_handle) => {
                // Closes the gap between "watches are live" and "the snapshot installed above
                // was already stale by the time that happened" — see `meshd`'s identical fix for
                // the full reasoning (a file changed during `spawn`'s registration window has no
                // other mechanism to ever be noticed afterward).
                mesh_server::FileWatcherService::execute_reload_sync(&watcher_state);
            }
            Err(e) => {
                tracing::warn!(target: "mesh::watcher", "Failed to start FileWatcherService: {e}");
            }
        }
    });

    run_server(state, cancel_token).await?;
    Ok(())
}

#[cfg(test)]
mod daemon_log_tests {
    use super::rotate_and_open_log;
    use std::io::Write;

    #[test]
    fn first_open_creates_the_current_log_with_no_rotation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut f = rotate_and_open_log(dir.path(), "ws", 3).expect("open");
        write!(f, "hello").expect("write");
        assert!(dir.path().join("meshd-ws.log").exists());
        assert!(!dir.path().join("meshd-ws.log.1").exists());
    }

    #[test]
    fn reopening_rotates_the_previous_log_instead_of_overwriting_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut f1 = rotate_and_open_log(dir.path(), "ws", 3).expect("open 1");
        write!(f1, "run 1").expect("write 1");
        drop(f1);

        let mut f2 = rotate_and_open_log(dir.path(), "ws", 3).expect("open 2");
        write!(f2, "run 2").expect("write 2");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-ws.log.1")).expect("read .1"),
            "run 1",
            "the previous run's log must survive as .1, not be silently overwritten"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-ws.log")).expect("read current"),
            "run 2"
        );
    }

    #[test]
    fn old_rotations_shift_up_and_the_oldest_is_eventually_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        // With max_rotations=2, the retained set is [current, .1, .2] — 3 generations. The 4th
        // open is the first one where something (run 1, by then in the .2 slot) must be deleted
        // rather than shifted further, since there is no .3 slot to shift it into.
        for content in ["run 1", "run 2", "run 3", "run 4"] {
            let mut f = rotate_and_open_log(dir.path(), "ws", 2).expect("open");
            write!(f, "{content}").expect("write");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-ws.log")).expect("read current"),
            "run 4"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-ws.log.1")).expect("read .1"),
            "run 3"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-ws.log.2")).expect("read .2"),
            "run 2"
        );
        assert!(
            !dir.path().join("meshd-ws.log.3").exists(),
            "max_rotations=2 must never retain a .3 generation"
        );
    }

    #[test]
    fn different_workspace_ids_never_share_a_log_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut a = rotate_and_open_log(dir.path(), "workspace-a", 3).expect("open a");
        write!(a, "from a").expect("write a");
        let mut b = rotate_and_open_log(dir.path(), "workspace-b", 3).expect("open b");
        write!(b, "from b").expect("write b");

        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-workspace-a.log")).expect("read a"),
            "from a"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("meshd-workspace-b.log")).expect("read b"),
            "from b"
        );
    }
}

#[cfg(all(test, unix))]
mod proxy_tests {
    use super::{bridge, try_proxy_unix, ProxyAttempt, ProxyEnd};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Plan 4 step 4.13 (a): the daemon answered `ensure_daemon_running`, then
    /// went away before the proxy connected. The session falls back to
    /// standalone instead of ending on an EOF.
    #[tokio::test]
    async fn connect_failure_after_successful_ensure_falls_back_to_standalone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        // What `ensure_daemon_running` saw: a live daemon.
        assert!(tokio::net::UnixStream::connect(&sock).await.is_ok());
        drop(listener);
        std::fs::remove_file(&sock).expect("rm socket");

        let attempt = try_proxy_unix(Ok(()), &sock, "ws").await.expect("no error");
        assert_eq!(attempt, ProxyAttempt::Fallback);
    }

    /// A daemon closing its end first is reported as such (the caller logs it).
    #[tokio::test]
    async fn daemon_closing_first_is_reported_as_daemon_closed() {
        let (daemon_side, proxy_side) = tokio::io::duplex(64);
        let (client_stdin, _client_keeps_stdin_open) = tokio::io::duplex(64);
        let (mut client_stdout_reader, client_stdout) = tokio::io::duplex(64);
        drop(daemon_side);
        let (r, w) = tokio::io::split(proxy_side);
        let end = bridge(r, w, client_stdin, client_stdout).await;
        assert_eq!(end, ProxyEnd::DaemonClosed);
        let mut out = Vec::new();
        client_stdout_reader
            .read_to_end(&mut out)
            .await
            .expect("read");
        assert!(out.is_empty());
    }

    /// The client closing stdin first drains the daemon's response and is a
    /// normal end, not a daemon failure.
    #[tokio::test]
    async fn client_closing_stdin_drains_daemon_and_ends_cleanly() {
        let (mut daemon_side, proxy_side) = tokio::io::duplex(64);
        let (client_stdin, client_stdin_writer) = tokio::io::duplex(64);
        let (mut client_stdout_reader, client_stdout) = tokio::io::duplex(64);
        drop(client_stdin_writer);
        let daemon = tokio::spawn(async move {
            let mut req = Vec::new();
            daemon_side.read_to_end(&mut req).await.expect("read");
            daemon_side.write_all(b"{\"id\":1}\n").await.expect("write");
        });
        let (r, w) = tokio::io::split(proxy_side);
        let end = bridge(r, w, client_stdin, client_stdout).await;
        daemon.await.expect("daemon");
        assert_eq!(end, ProxyEnd::ClientClosed);
        let mut out = Vec::new();
        client_stdout_reader
            .read_to_end(&mut out)
            .await
            .expect("read");
        assert_eq!(out, b"{\"id\":1}\n");
    }
}
