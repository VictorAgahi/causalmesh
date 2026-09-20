use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("I/O error during audit log operation: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Hash chain broken at entry {0}: expected {1}, got {2}")]
    BrokenChain(u64, String, String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub entry_seq: u64,
    pub prev_hash: String,
    pub timestamp: String,
    pub session_id: String,
    pub trace_id: Option<String>,
    pub tool: String,
    pub args_digest: String,
    pub status: String,
    pub files_accessed: Vec<String>,
    pub secrets_redacted_count: usize,
    pub entry_hash: String,
}

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

struct AdvisoryFileLockGuard {
    #[cfg(unix)]
    fd: std::os::unix::io::RawFd,
}

impl AdvisoryFileLockGuard {
    fn lock(file: &File) -> Self {
        #[cfg(unix)]
        {
            let fd = file.as_raw_fd();
            unsafe {
                libc::flock(fd, libc::LOCK_EX);
            }
            Self { fd }
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Self {}
        }
    }
}

impl Drop for AdvisoryFileLockGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
    }
}

pub struct AuditLogger {
    file: Mutex<File>,
    last_hash: Mutex<String>,
    seq_counter: Mutex<u64>,
    log_path: PathBuf,
}

impl AuditLogger {
    pub const GENESIS_HASH: &'static str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    pub fn default_log_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".cache")
            .join("mesh-mcp")
            .join("audit.log")
    }

    pub fn new(path: Option<PathBuf>) -> Result<Self, AuditError> {
        let log_path = path.unwrap_or_else(Self::default_log_path);

        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }

        let mut last_hash = Self::GENESIS_HASH.to_string();
        let mut seq_counter = 0u64;

        if log_path.exists() {
            let existing_file = File::open(&log_path)?;
            let reader = BufReader::new(existing_file);
            for line in reader.lines() {
                let line_str = line?;
                if line_str.trim().is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<AuditEntry>(&line_str) {
                    last_hash = entry.entry_hash;
                    seq_counter = entry.entry_seq + 1;
                }
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }

        Ok(Self {
            file: Mutex::new(file),
            last_hash: Mutex::new(last_hash),
            seq_counter: Mutex::new(seq_counter),
            log_path,
        })
    }

    #[inline]
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn compute_sha256(data: &[u8]) -> String {
        let mut context = Context::new(&SHA256);
        context.update(data);
        let digest = context.finish();
        hex::encode(digest.as_ref())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_entry(
        &self,
        session_id: &str,
        trace_id: Option<&str>,
        tool: &str,
        args_json: &str,
        status: &str,
        files_accessed: Vec<String>,
        secrets_redacted_count: usize,
    ) -> Result<AuditEntry, AuditError> {
        let mut seq_guard = self
            .seq_counter
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger mutex poisoned"))?;
        let mut hash_guard = self
            .last_hash
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger mutex poisoned"))?;

        let seq = *seq_guard;
        let prev_hash = hash_guard.clone();
        let timestamp = chrono_fallback_utc_now();
        let args_digest = Self::compute_sha256(args_json.as_bytes());

        // Hash_n = SHA256(Hash_{n-1} || Timestamp || SessionId || Tool || Digest)
        let hash_input = format!("{prev_hash}{timestamp}{session_id}{tool}{args_digest}");
        let entry_hash = Self::compute_sha256(hash_input.as_bytes());

        let entry = AuditEntry {
            entry_seq: seq,
            prev_hash,
            timestamp,
            session_id: session_id.to_string(),
            trace_id: trace_id.map(String::from),
            tool: tool.to_string(),
            args_digest,
            status: status.to_string(),
            files_accessed,
            secrets_redacted_count,
            entry_hash: entry_hash.clone(),
        };

        let serialized = serde_json::to_string(&entry)?;
        let mut file_guard = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger file mutex poisoned"))?;

        // Commandment 7: Acquire advisory OS file lock for multi-instance process safety
        let _advisory_lock = AdvisoryFileLockGuard::lock(&file_guard);

        writeln!(file_guard, "{serialized}")?;
        file_guard.flush()?;

        *hash_guard = entry_hash;
        *seq_guard += 1;

        Ok(entry)
    }

    pub fn verify_log_file(path: &Path) -> Result<bool, AuditError> {
        if !path.exists() {
            return Ok(true);
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut expected_prev_hash = Self::GENESIS_HASH.to_string();

        for line in reader.lines() {
            let line_str = line?;
            if line_str.trim().is_empty() {
                continue;
            }

            let entry: AuditEntry = serde_json::from_str(&line_str)?;
            if entry.prev_hash != expected_prev_hash {
                return Err(AuditError::BrokenChain(
                    entry.entry_seq,
                    expected_prev_hash,
                    entry.prev_hash,
                ));
            }

            let hash_input = format!(
                "{}{}{}{}{}",
                entry.prev_hash, entry.timestamp, entry.session_id, entry.tool, entry.args_digest
            );
            let calculated_hash = Self::compute_sha256(hash_input.as_bytes());
            if calculated_hash != entry.entry_hash {
                return Err(AuditError::BrokenChain(
                    entry.entry_seq,
                    calculated_hash,
                    entry.entry_hash,
                ));
            }

            expected_prev_hash = entry.entry_hash;
        }

        Ok(true)
    }
}

// Internal zero-dependency hex encoder
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            use std::fmt::Write;
            let _ = write!(s, "{:02x}", b);
        }
        s
    }
}

// Fallback ISO 8601 UTC timestamp generator without extra chrono dependency
fn chrono_fallback_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    // ISO 8601 format
    format!("{secs}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_hash_chain() {
        let temp_dir = std::env::temp_dir().join("mesh_audit_test");
        let log_file = temp_dir.join("test_audit.log");
        let _ = std::fs::remove_file(&log_file);

        let logger = AuditLogger::new(Some(log_file.clone())).expect("failed to init logger");
        let entry1 = logger
            .record_entry(
                "sess-1",
                Some("trace-abc"),
                "smart_search",
                "{\"query\":\"test\"}",
                "SUCCESS",
                vec!["src/main.rs".to_string()],
                0,
            )
            .expect("record entry 1");

        assert_eq!(entry1.prev_hash, AuditLogger::GENESIS_HASH);

        let entry2 = logger
            .record_entry(
                "sess-1",
                Some("trace-abc"),
                "find_dependents",
                "{\"target\":\"User\"}",
                "SUCCESS",
                vec![],
                1,
            )
            .expect("record entry 2");

        assert_eq!(entry2.prev_hash, entry1.entry_hash);

        let is_valid = AuditLogger::verify_log_file(&log_file).expect("verify chain");
        assert!(is_valid);
    }

    #[test]
    fn test_audit_concurrent_appends() {
        use std::sync::Arc;
        use std::thread;

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let log_file = temp_dir.path().join("concurrent_audit.log");

        let logger = Arc::new(AuditLogger::new(Some(log_file.clone())).expect("init logger"));
        let mut handles = Vec::new();

        // Spawn 8 concurrent threads appending entries simultaneously
        for t_idx in 0..8 {
            let log_clone = Arc::clone(&logger);
            let handle = thread::spawn(move || {
                for entry_idx in 0..15 {
                    let _ = log_clone.record_entry(
                        &format!("session-{t_idx}"),
                        Some("trace-concurrent"),
                        "smart_search",
                        &format!("{{\"thread\":{t_idx},\"seq\":{entry_idx}}}"),
                        "SUCCESS",
                        vec!["src/lib.rs".to_string()],
                        0,
                    );
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().expect("thread failed");
        }

        // Entire concurrent log (120 entries) must have unbroken SHA-256 hash chain
        let valid = AuditLogger::verify_log_file(&log_file).expect("verify log");
        assert!(valid, "Concurrent audit log hash chain was broken!");
    }
}
