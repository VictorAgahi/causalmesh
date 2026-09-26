#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use clap::{Parser, Subcommand};
use mesh_core::{AppState, AuditLogger, BackgroundRescanEngine, PersistentIndexCache};
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
                match ensure_daemon_running(&sock_path, &canonical_base, cli.config.as_deref())
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            target: "mesh::proxy",
                            "Connecting to meshd at {} (workspace {})",
                            sock_path.display(),
                            workspace_id
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
                let pipe_name = mesh_core::pipe_name_for(&workspace_id);
                match ensure_daemon_running_windows(
                    &pipe_name,
                    &canonical_base,
                    cli.config.as_deref(),
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!(
                            target: "mesh::proxy",
                            "Connecting to meshd at {} (workspace {})",
                            pipe_name,
                            workspace_id
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
            let _ = (standalone, workspace_id);

            // ── Standalone Mode (in-process fallback) ─────────────────────────
            run_standalone(cli.config.as_deref()).await?;
        }
    }

    Ok(())
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

    // Persistent, content-hash-keyed cache of tree-sitter `FileIndex` fragments (P2 step 3.2):
    // an unchanged file under an unchanged config is a SQLite lookup instead of a re-parse on
    // this cold start. A cache the process can't open (permissions, disk full) degrades to
    // "parse everything" rather than failing the boot — it's a performance optimization, never
    // a correctness dependency.
    let index_cache = match PersistentIndexCache::open(None) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!(
                target: "mesh::indexer",
                "Failed to open persistent index cache, continuing without it: {e}"
            );
            None
        }
    };

    // Single parallel scan over all roots, one reconcile, one atomic install.
    let snapshot = {
        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());
        WorkspaceIndexer::build_snapshot(
            &state.config,
            &state.allowed_roots,
            None,
            Some(&mut vfs),
            index_cache.as_ref(),
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
