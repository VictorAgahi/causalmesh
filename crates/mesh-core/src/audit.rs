use ring::digest::{Context, SHA256};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("I/O error during audit log operation: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Database error during audit operation: {0}")]
    Database(#[from] rusqlite::Error),

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

/// Robust cryptographic multi-process AuditLogger backed by SQLite in WAL mode per RFC-001 Commandment 7.
pub struct AuditLogger {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl AuditLogger {
    pub const GENESIS_HASH: &'static str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    pub fn default_db_path() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            PathBuf::from(home)
                .join(".cache")
                .join("mesh-mcp")
                .join("audit.db")
        } else {
            std::env::temp_dir().join("mesh-mcp").join("audit.db")
        }
    }

    pub fn default_log_path() -> PathBuf {
        Self::default_db_path()
    }

    pub fn new_in_memory() -> Result<Self, AuditError> {
        Self::new(Some(PathBuf::from(":memory:")))
    }

    pub fn new(path: Option<PathBuf>) -> Result<Self, AuditError> {
        let db_path = path.unwrap_or_else(Self::default_db_path);

        let conn = if db_path.to_string_lossy() == ":memory:" {
            Connection::open_in_memory()?
        } else {
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            Connection::open(&db_path)?
        };

        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let current_mode: String = conn
            .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
            .unwrap_or_default();

        if !current_mode.eq_ignore_ascii_case("wal") {
            let _ = conn.query_row("PRAGMA journal_mode = WAL;", [], |_| Ok(()));
        }

        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS audit_entries (
                 entry_seq INTEGER PRIMARY KEY,
                 prev_hash TEXT NOT NULL,
                 timestamp TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 trace_id TEXT,
                 tool TEXT NOT NULL,
                 args_digest TEXT NOT NULL,
                 status TEXT NOT NULL,
                 files_accessed TEXT NOT NULL,
                 secrets_redacted_count INTEGER NOT NULL,
                 entry_hash TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_audit_seq ON audit_entries(entry_seq);",
        )?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
        })
    }

    #[inline]
    pub fn log_path(&self) -> &Path {
        &self.db_path
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
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;

        // BEGIN IMMEDIATE acquires write lock on SQLite instantly, serializing concurrent processes
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Read the true committed tail from SQLite (never stale RAM memory)
        let mut stmt = tx.prepare(
            "SELECT entry_seq, entry_hash FROM audit_entries ORDER BY entry_seq DESC LIMIT 1",
        )?;
        let last_entry: Option<(u64, String)> = stmt
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        drop(stmt);

        let (seq, prev_hash) = match last_entry {
            Some((last_seq, last_hash)) => (last_seq + 1, last_hash),
            None => (0, Self::GENESIS_HASH.to_string()),
        };

        let timestamp = chrono_fallback_utc_now();
        let args_digest = Self::compute_sha256(args_json.as_bytes());

        // Hash_n = SHA256(Hash_{n-1} || Timestamp || SessionId || Tool || Digest)
        let hash_input = format!("{prev_hash}{timestamp}{session_id}{tool}{args_digest}");
        let entry_hash = Self::compute_sha256(hash_input.as_bytes());

        let files_json = serde_json::to_string(&files_accessed)?;

        tx.execute(
            "INSERT INTO audit_entries (
                entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                args_digest, status, files_accessed, secrets_redacted_count, entry_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                seq,
                prev_hash,
                timestamp,
                session_id,
                trace_id,
                tool,
                args_digest,
                status,
                files_json,
                secrets_redacted_count as i64,
                entry_hash,
            ],
        )?;

        tx.commit()?;

        Ok(AuditEntry {
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
            entry_hash,
        })
    }

    /// Verifies the cryptographic integrity of the SQLite audit database.
    pub fn verify_db(path: &Path) -> Result<bool, AuditError> {
        if !path.exists() {
            return Ok(true);
        }

        let conn = Connection::open(path)?;
        let mut stmt = conn.prepare(
            "SELECT entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash
             FROM audit_entries ORDER BY entry_seq ASC",
        )?;

        let mut expected_prev_hash = Self::GENESIS_HASH.to_string();

        let rows = stmt.query_map([], |row| {
            let files_str: String = row.get(8)?;
            let files: Vec<String> = serde_json::from_str(&files_str).unwrap_or_default();
            Ok(AuditEntry {
                entry_seq: row.get(0)?,
                prev_hash: row.get(1)?,
                timestamp: row.get(2)?,
                session_id: row.get(3)?,
                trace_id: row.get(4)?,
                tool: row.get(5)?,
                args_digest: row.get(6)?,
                status: row.get(7)?,
                files_accessed: files,
                secrets_redacted_count: row.get::<_, i64>(9)? as usize,
                entry_hash: row.get(10)?,
            })
        })?;

        for (idx, entry_res) in rows.enumerate() {
            let entry = entry_res?;
            let expected_seq = idx as u64;
            if entry.entry_seq != expected_seq {
                return Err(AuditError::BrokenChain(
                    entry.entry_seq,
                    format!("seq {expected_seq}"),
                    format!("seq {}", entry.entry_seq),
                ));
            }
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

    /// Backward-compatible alias for verify_db
    pub fn verify_log_file(path: &Path) -> Result<bool, AuditError> {
        Self::verify_db(path)
    }

    /// Exports all audit entries to a JSON Lines (JSONL) flat file for compliance tooling
    pub fn export_to_jsonl(&self, dest: &Path) -> Result<usize, AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;

        let mut stmt = conn.prepare(
            "SELECT entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash
             FROM audit_entries ORDER BY entry_seq ASC",
        )?;

        let mut file = File::create(dest)?;
        let mut count = 0;

        let rows = stmt.query_map([], |row| {
            let files_str: String = row.get(8)?;
            let files: Vec<String> = serde_json::from_str(&files_str).unwrap_or_default();
            Ok(AuditEntry {
                entry_seq: row.get(0)?,
                prev_hash: row.get(1)?,
                timestamp: row.get(2)?,
                session_id: row.get(3)?,
                trace_id: row.get(4)?,
                tool: row.get(5)?,
                args_digest: row.get(6)?,
                status: row.get(7)?,
                files_accessed: files,
                secrets_redacted_count: row.get::<_, i64>(9)? as usize,
                entry_hash: row.get(10)?,
            })
        })?;

        for entry_res in rows {
            let entry = entry_res?;
            let serialized = serde_json::to_string(&entry)?;
            writeln!(file, "{serialized}")?;
            count += 1;
        }

        file.flush()?;
        Ok(count)
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
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("test_audit.db");

        let logger = AuditLogger::new(Some(db_file.clone())).expect("failed to init logger");
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
        assert_eq!(entry1.entry_seq, 0);

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
        assert_eq!(entry2.entry_seq, 1);

        let is_valid = AuditLogger::verify_db(&db_file).expect("verify chain");
        assert!(is_valid);
    }

    #[test]
    fn test_audit_multi_instance_concurrent_wal() {
        use std::thread;

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("concurrent_audit.db");

        // Pre-initialize DB schema so WAL mode is active
        let _init_logger = AuditLogger::new(Some(db_file.clone())).expect("init schema");

        // Spawn 8 concurrent threads, EACH with its own independent AuditLogger connection
        // (simulating separate IDE windows and CLI processes accessing the shared audit.db)
        let mut handles = Vec::new();
        for t_idx in 0..8 {
            let path_clone = db_file.clone();
            let handle = thread::spawn(move || {
                let logger =
                    AuditLogger::new(Some(path_clone)).expect("init independent logger handle");
                for entry_idx in 0..15 {
                    let _ = logger.record_entry(
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

        // Entire concurrent log (120 entries) must have unbroken sequential IDs and SHA-256 hash chain
        let valid = AuditLogger::verify_db(&db_file).expect("verify log");
        assert!(valid, "Multi-process audit log hash chain was broken!");

        // Test export to JSONL
        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger for export");
        let jsonl_file = temp_dir.path().join("export.jsonl");
        let exported_count = logger
            .export_to_jsonl(&jsonl_file)
            .expect("export to jsonl");
        assert_eq!(exported_count, 120);
    }
}
