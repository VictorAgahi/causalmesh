//! Socket path resolution and lifecycle management for the meshd Unix Domain Socket.
//!
//! Re-exports canonical implementation from `mesh_core::socket`.

pub use mesh_core::socket::{cleanup_stale_socket, socket_path_for, workspace_id};

#[cfg(windows)]
pub use mesh_core::socket::pipe_name_for;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_socket_path_for_env_override() {
        std::env::set_var("MESH_SOCKET_PATH", "/tmp/test-mesh.sock");
        assert_eq!(
            socket_path_for("deadbeef"),
            PathBuf::from("/tmp/test-mesh.sock")
        );
        std::env::remove_var("MESH_SOCKET_PATH");
    }

    #[test]
    fn test_workspace_id_differs_by_base_dir_and_is_stable() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");

        let id_a1 = workspace_id(dir_a.path());
        let id_a2 = workspace_id(dir_a.path());
        let id_b = workspace_id(dir_b.path());

        assert_eq!(id_a1, id_a2, "same workspace must always yield the same id");
        assert_ne!(
            id_a1, id_b,
            "two different workspaces must never share a socket"
        );
    }

    #[test]
    fn test_cleanup_stale_noop_if_absent() {
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
