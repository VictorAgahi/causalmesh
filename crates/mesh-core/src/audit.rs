use ring::digest::{Context, SHA256};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
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
    /// Hash-chain formula version this row was written (and must be verified) under.
    /// See `AuditLogger::CHAIN_VERSION`.
    pub chain_version: i64,
}

/// Robust cryptographic multi-process AuditLogger backed by SQLite in WAL mode per RFC-001 Commandment 7.
pub struct AuditLogger {
    conn: Mutex<Connection>,
    db_path: PathBuf,
}

impl AuditLogger {
    pub const GENESIS_HASH: &'static str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    /// Hash-chain formula version.
    ///
    /// v1 (legacy) hashed only `prev_hash || timestamp || session_id || tool || args_digest`,
    /// which left `status`, `files_accessed` and `secrets_redacted_count` mutable without
    /// breaking verification. v2 folds those three fields into the hash.
    ///
    /// Rows carry their own `chain_version` so pre-existing databases keep verifying under
    /// the formula they were written with (`verify_db` dispatches per row) instead of having
    /// every historical entry rejected as tampered the moment this binary is upgraded.
    pub const CHAIN_VERSION: i64 = 2;

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
                 entry_hash TEXT NOT NULL,
                 chain_version INTEGER NOT NULL DEFAULT 1
             );
             -- `entry_seq` is INTEGER PRIMARY KEY, i.e. the rowid: it is already the
             -- table's B-tree key. The secondary index earlier versions created on it
             -- only doubled every insert's write cost.
             DROP INDEX IF EXISTS idx_audit_seq;",
        )?;

        // Databases created before `chain_version` existed have the table but not the
        // column; `CREATE TABLE IF NOT EXISTS` above is a no-op for them. Add it here so
        // their rows are explicitly tagged v1 (the formula they were actually hashed
        // with) and keep verifying, rather than being silently reinterpreted under v2.
        let has_chain_version = conn
            .prepare("PRAGMA table_info(audit_entries)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|name| name.ok())
            .any(|name| name == "chain_version");
        if !has_chain_version {
            // Multiple processes/threads can race to open the same on-disk database and
            // all observe the column as missing before any of them adds it. SQLite has
            // no `ADD COLUMN IF NOT EXISTS`, so tolerate "duplicate column" specifically
            // (another opener won the race) and propagate anything else.
            if let Err(err) = conn.execute(
                "ALTER TABLE audit_entries ADD COLUMN chain_version INTEGER NOT NULL DEFAULT 1",
                [],
            ) {
                let is_duplicate_column = matches!(
                    &err,
                    rusqlite::Error::SqliteFailure(_, Some(msg))
                        if msg.contains("duplicate column name")
                );
                if !is_duplicate_column {
                    return Err(err.into());
                }
            }
        }

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

        // Read the true committed tail from SQLite (never stale RAM memory): another
        // process may share this DB. Cheap — `entry_seq` is the rowid, so this is a
        // single B-tree descent. Statements are cached across calls.
        let mut stmt = tx.prepare_cached(
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
        let files_json = serde_json::to_string(&files_accessed)?;

        // Hash_n = SHA256(Hash_{n-1} || Timestamp || SessionId || Tool || Digest ||
        //                 Status || FilesAccessed || SecretsRedactedCount)   [chain v2]
        //
        // v1 covered only the first five fields, which let `status`, `files_accessed`
        // and `secrets_redacted_count` be tampered with in the row without breaking the
        // chain. All new writes use v2; see `CHAIN_VERSION`.
        let hash_input = format!(
            "{prev_hash}{timestamp}{session_id}{tool}{args_digest}{status}{files_json}{secrets_redacted_count}"
        );
        let entry_hash = Self::compute_sha256(hash_input.as_bytes());

        tx.prepare_cached(
            "INSERT INTO audit_entries (
                entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                chain_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?
        .execute(params![
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
            Self::CHAIN_VERSION,
        ])?;

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
            chain_version: Self::CHAIN_VERSION,
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
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                    chain_version
             FROM audit_entries ORDER BY entry_seq ASC",
        )?;

        let mut expected_prev_hash = Self::GENESIS_HASH.to_string();

        // Raw `files_accessed` JSON text is kept alongside the parsed `Vec<String>` so
        // hashing below re-hashes the exact bytes that were hashed at insert time,
        // rather than a value re-serialized from the parsed form (which could differ,
        // e.g. in key/whitespace formatting, without the row being tampered with).
        let rows = stmt.query_map([], |row| {
            let files_str: String = row.get(8)?;
            let files: Vec<String> = serde_json::from_str(&files_str).unwrap_or_default();
            let chain_version: i64 = row.get(11)?;
            Ok((
                AuditEntry {
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
                    chain_version,
                },
                files_str,
            ))
        })?;

        for (idx, entry_res) in rows.enumerate() {
            let (entry, files_str) = entry_res?;
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

            // Verify each row under the hash formula it was actually written with, so
            // rows from a database created before `chain_version` existed (tagged v1 by
            // the migration in `new()`) keep verifying instead of being rejected the
            // moment this binary upgrades. See `CHAIN_VERSION`.
            let hash_input = match entry.chain_version {
                1 => format!(
                    "{}{}{}{}{}",
                    entry.prev_hash,
                    entry.timestamp,
                    entry.session_id,
                    entry.tool,
                    entry.args_digest
                ),
                _ => format!(
                    "{}{}{}{}{}{}{}{}",
                    entry.prev_hash,
                    entry.timestamp,
                    entry.session_id,
                    entry.tool,
                    entry.args_digest,
                    entry.status,
                    files_str,
                    entry.secrets_redacted_count
                ),
            };
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

    /// Reads audit entries for local, read-only reporting (`mesh-mcp stats`).
    ///
    /// Opens the database `SQLITE_OPEN_READ_ONLY` so this never contends with the
    /// write lock `record_entry` takes, and never mutates the file (mode stays
    /// whatever it was, normally `0600`). `since_epoch_secs`, when set, drops any
    /// entry older than that many seconds since the Unix epoch; `None` returns the
    /// full history. This performs no network I/O — the data never leaves the
    /// machine.
    pub fn read_entries(
        path: &Path,
        since_epoch_secs: Option<f64>,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        if !path.exists() {
            return Ok(Vec::new());
        }

        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut stmt = conn.prepare(
            "SELECT entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                    chain_version
             FROM audit_entries ORDER BY entry_seq ASC",
        )?;

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
                chain_version: row.get(11)?,
            })
        })?;

        let mut out = Vec::new();
        for entry_res in rows {
            let entry = entry_res?;
            if let Some(cutoff) = since_epoch_secs {
                // Timestamps are written by `chrono_fallback_utc_now` as
                // "{unix_secs}.{millis:03}Z" — strip the trailing 'Z' and parse
                // the epoch-seconds float directly rather than pulling in a
                // date/time parser for this one call site.
                let entry_secs = parse_timestamp_to_epoch_secs(&entry.timestamp);
                if entry_secs < cutoff {
                    continue;
                }
            }
            out.push(entry);
        }

        Ok(out)
    }

    /// Exports all audit entries to a JSON Lines (JSONL) flat file for compliance tooling
    pub fn export_to_jsonl(&self, dest: &Path) -> Result<usize, AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;

        let mut stmt = conn.prepare(
            "SELECT entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                    chain_version
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
                chain_version: row.get(11)?,
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
    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let secs_per_day = 86400;
    let days = (total_secs / secs_per_day) as i64;
    let rem_secs = (total_secs % secs_per_day) as u32;

    let hours = rem_secs / 3600;
    let minutes = (rem_secs % 3600) / 60;
    let seconds = rem_secs % 60;

    // Civil day calculation (Howard Hinnant algorithm)
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

fn parse_timestamp_to_epoch_secs(ts: &str) -> f64 {
    let trimmed = ts.trim_end_matches('Z');
    if let Ok(v) = trimmed.parse::<f64>() {
        return v;
    }
    // Parse ISO 8601 format: YYYY-MM-DDTHH:MM:SS.sss
    if let Some((date_part, time_part)) = trimmed.split_once('T') {
        let date_parts: Vec<&str> = date_part.split('-').collect();
        let time_subparts: Vec<&str> = time_part.split('.').collect();
        if date_parts.len() == 3 && !time_subparts.is_empty() {
            let hms: Vec<&str> = time_subparts[0].split(':').collect();
            if hms.len() == 3 {
                let y: i64 = date_parts[0].parse().unwrap_or(1970);
                let m: u32 = date_parts[1].parse().unwrap_or(1);
                let d: u32 = date_parts[2].parse().unwrap_or(1);
                let h: u64 = hms[0].parse().unwrap_or(0);
                let min: u64 = hms[1].parse().unwrap_or(0);
                let s: u64 = hms[2].parse().unwrap_or(0);
                let millis: f64 = time_subparts
                    .get(1)
                    .and_then(|ms| ms.parse::<f64>().ok())
                    .unwrap_or(0.0)
                    / 1000.0;

                let y = if m <= 2 { y - 1 } else { y };
                let era = (if y >= 0 { y } else { y - 399 }) / 400;
                let yoe = (y - era * 400) as u32;
                let m_idx = if m > 2 { m - 3 } else { m + 9 };
                let doy = (153 * m_idx + 2) / 5 + d - 1;
                let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
                let days = era * 146097 + (doe as i64) - 719468;
                return (days as f64) * 86400.0 + (h * 3600 + min * 60 + s) as f64 + millis;
            }
        }
    }
    0.0
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

    /// Asserts that tampering with `status`, `files_accessed` or `secrets_redacted_count`
    /// is detected under chain v2 as a broken chain.
    #[test]
    fn test_tampering_with_status_breaks_chain() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("tamper_status.db");

        let logger = AuditLogger::new(Some(db_file.clone())).expect("failed to init logger");
        logger
            .record_entry(
                "sess-1",
                Some("trace-abc"),
                "smart_search",
                "{\"query\":\"test\"}",
                "SUCCESS",
                vec!["src/main.rs".to_string()],
                0,
            )
            .expect("record entry");

        // Sanity check: the untouched chain verifies.
        assert!(AuditLogger::verify_db(&db_file).expect("verify chain"));

        // Tamper with `status` directly in the database, leaving `entry_hash` untouched.
        let conn = Connection::open(&db_file).expect("reopen db");
        conn.execute(
            "UPDATE audit_entries SET status = 'FAILURE' WHERE entry_seq = 0",
            [],
        )
        .expect("tamper with status");
        drop(conn);

        let result = AuditLogger::verify_db(&db_file);
        assert!(
            matches!(result, Err(AuditError::BrokenChain(_, _, _))),
            "expected BrokenChain after tampering with status, got {result:?}"
        );
    }

    /// Rows written under chain v1 (no `status`/`files_accessed`/`secrets_redacted_count`
    /// coverage) must keep verifying under their original formula after upgrading to v2 —
    /// the migration must not silently break existing audit logs.
    #[test]
    fn test_legacy_v1_rows_still_verify() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("legacy_v1.db");

        // Build a v1-style row by hand: hash computed over only the first five fields,
        // with chain_version explicitly 1, simulating a database written before this fix.
        let conn = Connection::open(&db_file).expect("create db");
        conn.execute_batch(
            "CREATE TABLE audit_entries (
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
                 entry_hash TEXT NOT NULL,
                 chain_version INTEGER NOT NULL DEFAULT 1
             );",
        )
        .expect("create legacy schema");

        let prev_hash = AuditLogger::GENESIS_HASH.to_string();
        let timestamp = "1700000000.000Z".to_string();
        let session_id = "sess-legacy".to_string();
        let tool = "smart_search".to_string();
        let args_digest = AuditLogger::compute_sha256(b"{}");
        let hash_input = format!("{prev_hash}{timestamp}{session_id}{tool}{args_digest}");
        let entry_hash = AuditLogger::compute_sha256(hash_input.as_bytes());

        conn.execute(
            "INSERT INTO audit_entries (
                entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                chain_version
            ) VALUES (0, ?1, ?2, ?3, NULL, ?4, ?5, 'SUCCESS', '[]', 0, ?6, 1)",
            params![
                prev_hash,
                timestamp,
                session_id,
                tool,
                args_digest,
                entry_hash
            ],
        )
        .expect("insert legacy row");
        drop(conn);

        assert!(
            AuditLogger::verify_db(&db_file).expect("verify legacy chain"),
            "legacy v1 row must still verify under its original formula"
        );
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

    #[test]
    fn test_read_entries_all_and_since_filter() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("read_entries.db");

        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger");
        logger
            .record_entry(
                "sess-1",
                None,
                "smart_search",
                "{}",
                "SUCCESS",
                vec!["a.rs".to_string()],
                0,
            )
            .expect("entry 0");
        logger
            .record_entry("sess-1", None, "find_dependents", "{}", "ERROR", vec![], 0)
            .expect("entry 1");

        // No filter: both entries come back.
        let all = AuditLogger::read_entries(&db_file, None).expect("read all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].tool, "smart_search");
        assert_eq!(all[1].tool, "find_dependents");

        // A cutoff far in the future excludes everything.
        let future_cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            + 3600.0;
        let none = AuditLogger::read_entries(&db_file, Some(future_cutoff)).expect("read none");
        assert!(none.is_empty());

        // A cutoff far in the past keeps everything.
        let past_cutoff = 0.0;
        let all_again =
            AuditLogger::read_entries(&db_file, Some(past_cutoff)).expect("read all again");
        assert_eq!(all_again.len(), 2);
    }

    #[test]
    fn test_read_entries_missing_db_returns_empty() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("does-not-exist.db");
        let entries = AuditLogger::read_entries(&missing, None).expect("read missing db");
        assert!(entries.is_empty());
    }
}
