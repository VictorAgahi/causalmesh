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
        }
        return p;
    }

    if let Some(home) = home_dir() {
        let p = home.join(".cache").join("mesh").join(filename);
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return p;
    }

    let identifier = user_session_identifier();
    #[cfg(unix)]
    return PathBuf::from(format!("/tmp/mesh-{identifier}-{filename}"));
    #[cfg(not(unix))]
    return std::env::temp_dir().join(format!("mesh-{identifier}-{filename}"));
}

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
