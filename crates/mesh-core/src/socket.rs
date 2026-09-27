//! Socket path resolution and lifecycle management for the meshd Unix Domain Socket and Windows Named Pipes.
//!
//! Per RFC-001 Commandment 7, the socket lives at:
//!   1. `$MESH_SOCKET_PATH` (env override)
//!   2. `$XDG_RUNTIME_DIR/mesh/<name>` (Linux best practice)
//!   3. `~/.cache/mesh/<name>` (macOS / portable fallback)
//!   4. `/tmp/mesh-<uid>-<name>` (last resort)
//!
//! `<name>` is `meshd.sock` for the legacy, workspace-agnostic [`socket_path`],
//! or `meshd-<workspace_id>.sock` for [`socket_path_for`] — see
//! [`workspace_id`] for why every workspace needs its own (idempotence
//! invariant I7: a response always concerns the workspace of the session
//! that asked, not whichever workspace's meshd happened to grab the shared
//! default socket first).

use std::path::{Path, PathBuf};

/// Derives a short, stable identifier for one workspace: the first 16 hex
/// characters of SHA-256(canonical `base_dir` + this binary's version).
/// Two different workspaces get distinct sockets, and so does the same
/// workspace indexed by two different `mesh-mcp` versions — an old, stale
/// daemon from before an upgrade is simply never found again rather than
/// silently serving newer clients from an outdated snapshot format.
pub fn workspace_id(base_dir: &Path) -> String {
    let canonical = dunce::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf());
    let key = format!("{}\u{0}{}", canonical.display(), env!("CARGO_PKG_VERSION"));
    let digest = crate::audit::AuditLogger::compute_sha256(key.as_bytes());
    digest[..16].to_string()
}

/// Resolves the workspace-scoped UDS socket path for `workspace_id` (see
/// [`workspace_id`]). This is the path a `mesh-mcp` proxy connects to and a
/// `meshd` for that same workspace binds — never the one-per-machine
/// [`socket_path`], which any two unrelated workspaces would otherwise race
/// to bind and then silently share.
pub fn socket_path_for(workspace_id: &str) -> PathBuf {
    resolve_socket_path(&format!("meshd-{workspace_id}.sock"))
}

/// Resolves the canonical, workspace-agnostic UDS socket path. Kept for
/// `MESH_SOCKET_PATH`-style explicit overrides and standalone/test use; a
/// real `mesh-mcp run` session resolves its daemon via [`socket_path_for`]
/// instead, scoped to the workspace it discovered.
pub fn socket_path() -> PathBuf {
    resolve_socket_path("meshd.sock")
}

fn resolve_socket_path(filename: &str) -> PathBuf {
    if let Ok(p) = std::env::var("MESH_SOCKET_PATH") {
        return PathBuf::from(p);
    }

    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(dir).join("mesh").join(filename);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
            chmod_owner_only(parent);
        }
        return p;
    }

    if let Some(home) = home_dir() {
        let p = home.join(".cache").join("mesh").join(filename);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
            chmod_owner_only(parent);
        }
        return p;
    }

    let identifier = user_session_identifier();
    #[cfg(unix)]
    return PathBuf::from(format!("/tmp/mesh-{identifier}-{filename}"));
    #[cfg(not(unix))]
    return std::env::temp_dir().join(format!("mesh-{identifier}-{filename}"));
}

/// Owner-only (0700) on the socket's parent directory (plan 4 step 4.7): a
/// world- or group-readable directory would still list the socket file's name
/// (though not its contents), and on a shared machine that is more than a
/// UDS listener should reveal. Best-effort — a chmod failure here is not
/// fatal, since the socket itself still binds.
#[cfg(unix)]
fn chmod_owner_only(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn chmod_owner_only(_dir: &Path) {}

fn user_session_identifier() -> String {
    if let Ok(user) = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .or_else(|_| std::env::var("USERNAME"))
    {
        if !user.trim().is_empty() {
            return user.trim().to_string();
        }
    }

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        uid.to_string()
    }
    #[cfg(not(unix))]
    {
        "default".to_string()
    }
}

/// The daemon behind one socket, as `doctor` needs it: enough to compare
/// versions and to find (and, on `--fix`, stop) *this* daemon specifically —
/// never any other process (plan 4 step 4.7).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonMeta {
    pub pid: u32,
    pub version: String,
}

/// Path for one workspace's [`DaemonMeta`], keyed by `workspace_id` rather
/// than the socket path: a Windows named pipe address has no filesystem
/// representation to sit a sidecar file next to, so both transports share
/// this one convention instead of each needing their own.
pub fn daemon_meta_path(workspace_id: &str) -> PathBuf {
    crate::paths::mesh_cache_dir()
        .join("meshd")
        .join(format!("{workspace_id}.meta"))
}

/// Records this daemon's PID and `version` for `workspace_id`, right after its
/// socket or named pipe is ready to accept clients. Best-effort: a write
/// failure only means `doctor` cannot compare versions or offer `--fix` for
/// this daemon, never that the daemon itself should not start.
pub fn write_daemon_meta(workspace_id: &str, version: &str) {
    let meta = DaemonMeta {
        pid: std::process::id(),
        version: version.to_string(),
    };
    let Ok(json) = serde_json::to_string(&meta) else {
        return;
    };
    let path = daemon_meta_path(workspace_id);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    chmod_owner_only(parent);
    if std::fs::write(&path, json).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// Reads back the [`DaemonMeta`] `write_daemon_meta` wrote for `workspace_id`,
/// if present and well-formed.
pub fn read_daemon_meta(workspace_id: &str) -> Option<DaemonMeta> {
    let content = std::fs::read_to_string(daemon_meta_path(workspace_id)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Best-effort cleanup of `workspace_id`'s metadata file; safe to call whether
/// or not one exists.
pub fn remove_daemon_meta(workspace_id: &str) {
    let _ = std::fs::remove_file(daemon_meta_path(workspace_id));
}

/// Whether `pid` is still a live process. `false` on any error (already
/// exited, or this process cannot even see it) — callers treat "unknown" the
/// same as "gone", since it is a diagnostic hint, not a security boundary.
pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // Signal 0: no signal sent, only existence/permission checked. Kernel
        // enforced (EPERM for a PID this user does not own), so this can never
        // report a foreign-user's process as "ours".
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                false
            } else {
                CloseHandle(handle);
                true
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Best-effort graceful termination of `pid`. The OS itself is the only
/// enforcement of "same user" here (Unix `kill` fails with `EPERM` across
/// users; Windows `OpenProcess` fails without the right access token) — this
/// function adds no separate ownership check on top of that, and only ever
/// terminates the exact PID a [`DaemonMeta`] this same user's daemon wrote.
pub fn terminate_process(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let ok = TerminateProcess(handle, 1) != 0;
            CloseHandle(handle);
            ok
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Whether *something* is actually live and listening on `workspace_id`'s own
/// socket or named pipe right now (plan 4 step 4.7 review). A stale
/// [`DaemonMeta`] only ever names the real daemon's PID until a crash, an
/// unclean shutdown, or a reboot: after that its `pid` field is just a number
/// that may since have been reused by an unrelated process. Bare
/// [`process_is_alive`] cannot tell the two apart; this can, in practice —
/// meshd's socket path already encodes the workspace id, so an unrelated
/// process would have to both reuse the exact recycled PID *and* happen to be
/// bound to this exact, workspace-specific path, not merely exist. `doctor
/// --fix` only calls [`terminate_process`] when this also returns `true`,
/// narrowing (though on Windows, not fully eliminating, since a named pipe
/// instance is consumed by one connect — see the caller) the PID-reuse risk
/// to something that would require deliberate malice, already outside this
/// tool's threat model (the same trust boundary `kill`/`OpenProcess` already
/// rely on: same-machine, same-user processes).
pub fn daemon_is_reachable(workspace_id: &str) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(socket_path_for(workspace_id)).is_ok()
    }
    #[cfg(windows)]
    {
        // Not verified by compiling for Windows in this session (no MSVC
        // toolchain available) — same caveat as `terminate_process`'s Windows
        // branch. `File::open` on a `\\.\pipe\...` path performs the same
        // `CreateFile` connect a named pipe client uses.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name_for(workspace_id))
            .is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = workspace_id;
        false
    }
}

pub fn cleanup_stale_socket(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        if UnixStream::connect(path).is_err() {
            let _ = std::fs::remove_file(path);
            tracing::info!(
                target: "mesh::socket",
                "Removed stale socket at {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::remove_file(path);
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(windows)]
pub fn pipe_name() -> String {
    if let Ok(p) = std::env::var("MESH_PIPE_NAME") {
        return p;
    }

    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
    format!(r"\\.\pipe\mesh-mcp-{user}")
}

/// Workspace-scoped named pipe, mirroring [`socket_path_for`] for Windows.
#[cfg(windows)]
pub fn pipe_name_for(workspace_id: &str) -> String {
    if let Ok(p) = std::env::var("MESH_PIPE_NAME") {
        return p;
    }

    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
    format!(r"\\.\pipe\mesh-mcp-{user}-{workspace_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_path_env_override() {
        std::env::set_var("MESH_SOCKET_PATH", "/tmp/test-mesh.sock");
        assert_eq!(socket_path(), PathBuf::from("/tmp/test-mesh.sock"));
        std::env::remove_var("MESH_SOCKET_PATH");
    }

    #[test]
    fn test_cleanup_stale_noop_if_absent() {
        cleanup_stale_socket(Path::new("/tmp/nonexistent-mesh-test.sock"));
    }

    #[test]
    #[cfg(unix)]
    fn test_cleanup_stale_socket_removes_orphaned_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale-meshd.sock");

        {
            let listener =
                std::os::unix::net::UnixListener::bind(&path).expect("bind stale listener");
            drop(listener);
        }
        assert!(path.exists(), "socket file should remain after drop");

        cleanup_stale_socket(&path);

        assert!(
            !path.exists(),
            "stale socket file must be removed when nothing is listening"
        );

        let listener = std::os::unix::net::UnixListener::bind(&path);
        assert!(
            listener.is_ok(),
            "bind after stale-socket cleanup should succeed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_cleanup_stale_socket_preserves_live_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("live-meshd.sock");

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind live listener");

        cleanup_stale_socket(&path);

        assert!(
            path.exists(),
            "a live socket's file must not be removed by cleanup_stale_socket"
        );

        drop(listener);
    }
}
