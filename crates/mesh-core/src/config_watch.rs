//! Change detection for the config file a long-lived process was built from,
//! and the record `meshd` leaves for `doctor` about which config it serves.
//!
//! The check is cheap enough to run on every request: a `stat` decides whether
//! anything may have changed (mtime or length), and only then is the file read
//! and hashed. The hash is what decides, so a `touch` or a save without changes
//! never rebuilds a workspace.

use crate::index_cache::sha256;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// What a config file looked like when it was last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigStamp {
    /// Modification time in milliseconds since the Unix epoch (0 if unknown).
    pub mtime_ms: u128,
    pub len: u64,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `(mtime_ms, len)` of `path`, or `None` if it cannot be stat-ed.
pub fn config_fingerprint_cheap(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis());
    Some((mtime_ms, meta.len()))
}

/// Reads and hashes `path`.
pub fn stamp_config(path: &Path) -> std::io::Result<ConfigStamp> {
    let (mtime_ms, _) = config_fingerprint_cheap(path).unwrap_or((0, 0));
    let bytes = std::fs::read(path)?;
    Ok(ConfigStamp {
        mtime_ms,
        len: bytes.len() as u64,
        sha256: hex(&sha256(&bytes)),
    })
}

/// What `meshd` records about the config it serves, next to its
/// [`crate::socket::DaemonMeta`]: enough for `doctor` to say whether the running
/// daemon is behind the file on disk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonConfigStatus {
    /// When this daemon process started (ms since the Unix epoch).
    pub started_ms: u128,
    /// Absolute path of the config the daemon serves; `None` for the built-in
    /// default (no config file found).
    pub config_path: Option<PathBuf>,
    /// SHA-256 of the config bytes the serving workspace state was built from.
    pub config_sha256: Option<String>,
    /// A newer config on disk that failed to load, with the reason; the
    /// previous state keeps serving. Cleared by the next successful load.
    pub last_error: Option<String>,
}

/// Where [`DaemonConfigStatus`] of `workspace_id` lives.
pub fn daemon_config_status_path(workspace_id: &str) -> PathBuf {
    crate::paths::mesh_cache_dir()
        .join("meshd")
        .join(format!("{workspace_id}.config.json"))
}

/// Best-effort write (a failure only means `doctor` cannot compare).
pub fn write_daemon_config_status(workspace_id: &str, status: &DaemonConfigStatus) {
    let Ok(json) = serde_json::to_string(status) else {
        return;
    };
    let path = daemon_config_status_path(workspace_id);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // Written aside then renamed: `doctor` never reads half a record.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn read_daemon_config_status(workspace_id: &str) -> Option<DaemonConfigStatus> {
    let content = std::fs::read_to_string(daemon_config_status_path(workspace_id)).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn remove_daemon_config_status(workspace_id: &str) {
    let _ = std::fs::remove_file(daemon_config_status_path(workspace_id));
}

/// How a running daemon compares with the config on disk, as `doctor` words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDrift {
    /// The daemon serves the file as it is on disk.
    InSync,
    /// The file changed after the daemon built its state and it has not (yet)
    /// picked it up: a daemon from before 7.1.7, or one still rebuilding.
    Stale { reason: String },
    /// The file changed and the daemon refused it (invalid); the old state serves.
    Rejected { error: String },
}

/// Compares `status` (what the daemon recorded) with `config_path` on disk.
pub fn config_drift(status: &DaemonConfigStatus, config_path: &Path) -> ConfigDrift {
    if let Some(error) = &status.last_error {
        return ConfigDrift::Rejected {
            error: error.clone(),
        };
    }
    let Ok(now) = stamp_config(config_path) else {
        return ConfigDrift::InSync;
    };
    match &status.config_sha256 {
        Some(built) if *built == now.sha256 => ConfigDrift::InSync,
        Some(_) => ConfigDrift::Stale {
            reason: "built from a different config hash".to_string(),
        },
        None if status.started_ms < now.mtime_ms => ConfigDrift::Stale {
            reason: "started before the config file was last modified".to_string(),
        },
        None => ConfigDrift::InSync,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_changes_with_content_not_with_a_touch() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "a = 1\n").expect("write");
        let a = stamp_config(&p).expect("stamp");
        std::fs::write(&p, "a = 1\n").expect("rewrite same");
        assert_eq!(stamp_config(&p).expect("stamp").sha256, a.sha256);
        std::fs::write(&p, "a = 2\n").expect("rewrite");
        assert_ne!(stamp_config(&p).expect("stamp").sha256, a.sha256);
        assert_eq!(a.sha256.len(), 64);
    }

    #[test]
    fn drift_is_decided_by_hash_then_recorded_error() {
        let dir = tempfile::tempdir().expect("dir");
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "a = 1\n").expect("write");
        let mut status = DaemonConfigStatus {
            started_ms: 0,
            config_path: Some(p.clone()),
            config_sha256: Some(stamp_config(&p).expect("stamp").sha256),
            last_error: None,
        };
        assert_eq!(config_drift(&status, &p), ConfigDrift::InSync);
        std::fs::write(&p, "a = 2\n").expect("rewrite");
        assert!(matches!(
            config_drift(&status, &p),
            ConfigDrift::Stale { .. }
        ));
        status.last_error = Some("bad".to_string());
        assert_eq!(
            config_drift(&status, &p),
            ConfigDrift::Rejected {
                error: "bad".to_string()
            }
        );
    }
}
