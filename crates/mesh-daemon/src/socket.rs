//! Socket path resolution and lifecycle management for the meshd Unix Domain Socket.
//!
//! Per RFC-001 Commandment 7, the socket lives at:
//!   1. `$MESH_SOCKET_PATH` (env override)
//!   2. `$XDG_RUNTIME_DIR/mesh/meshd.sock` (Linux best practice)
//!   3. `~/.cache/mesh/meshd.sock` (macOS / portable fallback)
//!   4. `/tmp/mesh-<uid>.sock` (last resort)

use std::path::PathBuf;

/// Resolves the canonical UDS socket path for this user session.
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("MESH_SOCKET_PATH") {
        return PathBuf::from(p);
    }

    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(dir).join("mesh").join("meshd.sock");
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return p;
    }

    if let Some(home) = home_dir() {
        let p = home.join(".cache").join("mesh").join("meshd.sock");
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        return p;
    }

    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = unsafe { libc::getpid() as u32 };

    #[cfg(unix)]
    return PathBuf::from(format!("/tmp/mesh-{uid}.sock"));
    #[cfg(not(unix))]
    return std::env::temp_dir().join(format!("mesh-{uid}.sock"));
}

pub fn cleanup_stale_socket(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        if UnixStream::connect(path).is_err() {
            let _ = std::fs::remove_file(path);
            tracing::info!(
                target: "meshd::socket",
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
        // Should not panic on missing file
        cleanup_stale_socket(std::path::Path::new("/tmp/nonexistent-mesh-test.sock"));
    }

    #[test]
    #[cfg(unix)]
    fn test_cleanup_stale_socket_removes_orphaned_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale-meshd.sock");

        {
            let listener =
                std::os::unix::net::UnixListener::bind(&path).expect("bind stale listener");
            drop(listener); // orphaned: file remains, nothing is accepting.
        }
        assert!(path.exists(), "socket file should remain after drop");

        cleanup_stale_socket(&path);

        assert!(
            !path.exists(),
            "stale socket file must be removed when nothing is listening"
        );

        // The next daemon must now be able to bind the same path cleanly.
        let listener = std::os::unix::net::UnixListener::bind(&path);
        assert!(
            listener.is_ok(),
            "bind after stale-socket cleanup should succeed"
        );
    }

    /// A live, currently-listening socket must NOT be treated as stale.
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