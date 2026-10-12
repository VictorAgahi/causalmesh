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
    /// Sanitized error of an `ERROR` row, as a JSON object
    /// `{"class","code","message"}`; `None` for successful calls and for rows
    /// written before this column existed. Never raw arguments: see [`ErrorDetail`].
    #[serde(default)]
    pub error_detail: Option<String>,
    /// `SHA-256(entry_hash || error_detail)` binding the detail to its row; present
    /// exactly when `error_detail` is. See `AuditLogger::error_detail_sig`.
    #[serde(default)]
    pub error_detail_sig: Option<String>,
}

/// Longest stored error message, in characters.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 200;

/// Short, sanitized account of why a tool call failed, stored with its audit row
/// so field errors can be diagnosed without keeping the call's arguments.
///
/// The message has secrets masked (`[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]`),
/// absolute home directories collapsed to `~`, control characters flattened and
/// is cut to [`MAX_ERROR_MESSAGE_CHARS`] characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub class: String,
    pub code: i32,
    pub message: String,
}

impl ErrorDetail {
    pub fn sanitized(class: &str, code: i32, raw_message: &str) -> Self {
        let home = crate::root_guard::home_dir().map(|h| h.to_string_lossy().into_owned());
        Self::sanitized_with_home(class, code, raw_message, home.as_deref())
    }

    /// [`Self::sanitized`] with the home directory injected (tests).
    pub fn sanitized_with_home(
        class: &str,
        code: i32,
        raw_message: &str,
        home: Option<&str>,
    ) -> Self {
        Self {
            class: class.chars().take(40).collect(),
            code,
            message: sanitize_error_message(raw_message, home),
        }
    }

    /// The canonical stored form (the exact text bound by `error_detail_sig`).
    pub fn to_stored(&self) -> String {
        serde_json::json!({"class": self.class, "code": self.code, "message": self.message})
            .to_string()
    }

    /// Parses a stored `error_detail` value.
    pub fn from_stored(stored: &str) -> Option<Self> {
        serde_json::from_str(stored).ok()
    }
}

const REDACTED: &str = "[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]";
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "passphrase",
    "private",
    "jwt",
    "cert",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "signing_key",
    "authorization",
];
const SECRET_PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "ghs_", "xoxb-", "xoxp-", "AKIA"];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Characters of a base64 / base64url / JWT-like run (`.` separates JWT segments).
fn is_blob_char(c: char) -> bool {
    is_token_char(c) || matches!(c, '.' | '+' | '=')
}

/// A long run that looks like key material: 32+ letters and digits, or (with
/// `-` / `_` / `+` / `=`, as base64url and base64 use) 32+ characters mixing digits
/// with upper and lower case. All-lowercase hyphenated names (UUID directories,
/// slugs) are not.
fn looks_like_secret_blob(run: &str) -> bool {
    if run.len() < 32 {
        return false;
    }
    let digit = run.chars().any(|c| c.is_ascii_digit());
    let upper = run.chars().any(|c| c.is_ascii_uppercase());
    let lower = run.chars().any(|c| c.is_ascii_lowercase());
    if run.chars().all(|c| c.is_ascii_alphanumeric()) {
        digit && (upper || lower)
    } else {
        digit && upper && lower
    }
}

/// Byte ranges of secret values in `s` (key/value pairs, bearer tokens, well
/// known token prefixes, long mixed alphanumeric blobs), merged and ordered.
fn secret_ranges(s: &str) -> Vec<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    let value_end = |start: usize| -> usize {
        let mut end = start;
        for (off, c) in s[start..].char_indices() {
            if c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '&' | '}' | ')' | '>') {
                return start + off;
            }
            end = start + off + c.len_utf8();
        }
        end
    };

    // `key = value`, `key: value`, `"key": "value"`.
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(key) {
            let key_start = from + pos;
            let mut i = key_start + key.len();
            from = i;
            while i < bytes.len() && is_token_char(bytes[i] as char) {
                i += 1;
            }
            while i < bytes.len() && matches!(bytes[i], b'"' | b'\'' | b' ') {
                i += 1;
            }
            if i < bytes.len() && matches!(bytes[i], b'=' | b':') {
                i += 1;
                while i < bytes.len() && matches!(bytes[i], b' ' | b'"' | b'\'') {
                    i += 1;
                }
                let end = value_end(i);
                if end > i {
                    ranges.push((i, end));
                }
            }
        }
    }

    // `Bearer <token>`: only a token-looking word follows (prose such as "bearer of"
    // is kept); `Basic <token>`: a base64-looking credential, not an ordinary word.
    for (scheme, min_len) in [("bearer ", 16usize), ("basic ", 8)] {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(scheme) {
            let start = from + pos + scheme.len();
            from = start;
            let end = value_end(start);
            let word = &s[start..end];
            let credential_shaped = word.len() >= min_len
                && word.chars().all(|c| is_blob_char(c) || c == '/')
                && (word.chars().any(|c| c.is_ascii_digit())
                    || (word.chars().any(|c| c.is_ascii_uppercase())
                        && word.chars().any(|c| c.is_ascii_lowercase())));
            if credential_shaped {
                ranges.push((start, end));
            }
        }
    }

    // `scheme://user:password@host`: the whole userinfo goes.
    let mut from = 0;
    while let Some(pos) = s[from..].find("://") {
        let start = from + pos + 3;
        from = start;
        let authority_end = s[start..]
            .find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'' | ')') || c.is_whitespace())
            .map_or(s.len(), |o| start + o);
        if let Some(at) = s[start..authority_end].rfind('@') {
            if at > 0 {
                ranges.push((start, start + at));
            }
        }
    }

    // `user:password@host` whose `://` was lost (a path normalised to `https:/u:p@h`).
    for (at, _) in s.match_indices('@') {
        let token_start = s[..at]
            .rfind(|c: char| c.is_whitespace() || matches!(c, '/' | '"' | '\'' | '(' | '<'))
            .map_or(0, |i| i + 1);
        if s[token_start..at].contains(':') {
            ranges.push((token_start, at));
        }
    }

    // Well known token prefixes, JWTs and long mixed blobs, blob by blob.
    let mut idx = 0;
    while idx < s.len() {
        let rest = &s[idx..];
        let Some(first) = rest.chars().next() else {
            break;
        };
        if !is_blob_char(first) {
            idx += first.len_utf8();
            continue;
        }
        let len = rest
            .char_indices()
            .find(|(_, c)| !is_blob_char(*c))
            .map_or(rest.len(), |(o, _)| o);
        let blob = &rest[..len];
        let segments: Vec<&str> = blob.split('.').collect();
        let b64url = |t: &str| {
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        };
        let jwt = segments.len() == 3
            && blob.len() >= 20
            && segments.iter().all(|t| !t.is_empty() && b64url(t))
            && (segments[0].starts_with("eyJ") || segments.iter().all(|t| t.len() >= 10));
        if jwt || (blob.len() >= 16 && SECRET_PREFIXES.iter().any(|p| blob.starts_with(p))) {
            ranges.push((idx, idx + len));
        } else {
            let mut off = 0;
            for part in &segments {
                if looks_like_secret_blob(part) {
                    ranges.push((idx + off, idx + off + part.len()));
                }
                off += part.len() + 1;
            }
        }
        idx += len;
    }

    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// Collapses absolute home directories (`/Users/x`, `/home/x`, `C:\Users\x`, and
/// the current user's own home) to `~`.
fn collapse_home(s: &str, home: Option<&str>) -> String {
    let mut out = match home {
        Some(h) if h.len() > 1 => s.replace(h, "~"),
        _ => s.to_string(),
    };
    for marker in ["/Users/", "/home/", "\\Users\\"] {
        let windows = marker.starts_with('\\');
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(pos) = rest.find(marker) {
            let before = &rest[..pos];
            let at_token_start = before.chars().next_back().is_none_or(|c| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '(' | '=' | ':' | '`')
            });
            // A Windows drive prefix (`C:`) belongs to the collapsed part.
            let drive = windows
                && before.len() >= 2
                && before.as_bytes()[before.len() - 1] == b':'
                && before.as_bytes()[before.len() - 2].is_ascii_alphabetic();
            if !(at_token_start || drive) {
                result.push_str(&rest[..pos + marker.len()]);
                rest = &rest[pos + marker.len()..];
                continue;
            }
            result.push_str(if drive {
                &before[..before.len() - 2]
            } else {
                before
            });
            let after = &rest[pos + marker.len()..];
            let seg_end = after
                .find(|c: char| matches!(c, '/' | '\\' | '"' | '\'') || c.is_whitespace())
                .unwrap_or(after.len());
            result.push('~');
            rest = &after[seg_end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// Removes the user-supplied value serde and friends echo into their messages:
/// `invalid type: string "x", expected u32`, `invalid value: integer `5`, ...`,
/// `unknown variant `x`, expected one of ...`, `unknown field `x`, ...`. The quoted
/// value is dropped wholesale (a value containing `, expected` cannot leak its
/// remainder); the schema-side text (the type, the expected variants) is kept. An
/// unterminated quote drops the rest of the message.
fn strip_serde_values(raw: &str) -> String {
    const MARKERS: [&str; 4] = [
        "invalid type: ",
        "invalid value: ",
        "unknown variant ",
        "unknown field ",
    ];
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some((pos, marker)) = MARKERS
        .iter()
        .filter_map(|m| rest.find(m).map(|i| (i, *m)))
        .min_by_key(|(i, _)| *i)
    {
        let after = pos + marker.len();
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        // The value opens at the first quote, unless serde's own ", expected" comes
        // first (`invalid type: null, expected a string` echoes no value).
        let first_quote = tail.find(['"', '`']);
        let expected_at = tail.find(", expected");
        let quote = match (first_quote, expected_at) {
            (Some(q), Some(e)) if e < q => None,
            (q, _) => q,
        };
        let Some(q) = quote else {
            rest = tail;
            if expected_at.is_none() && first_quote.is_none() {
                break;
            }
            // Keep scanning past this marker.
            let keep = expected_at.unwrap_or(tail.len());
            out.push_str(&tail[..keep]);
            rest = &tail[keep..];
            continue;
        };
        out.push_str(&tail[..q]);
        out.push_str("<value>");
        let open = tail.as_bytes()[q];
        let body = &tail[q + 1..];
        let mut close = None;
        let mut escaped = false;
        for (off, c) in body.char_indices() {
            if open == b'"' && escaped {
                escaped = false;
            } else if open == b'"' && c == '\\' {
                escaped = true;
            } else if c as u32 == u32::from(open) {
                close = Some(off);
                break;
            }
        }
        match close {
            Some(c) => rest = &body[c + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Sanitizes a free-form error message for storage: redact, collapse home
/// directories, flatten whitespace, truncate to [`MAX_ERROR_MESSAGE_CHARS`].
pub fn sanitize_error_message(raw: &str, home: Option<&str>) -> String {
    let text = strip_serde_values(raw);

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for (a, b) in secret_ranges(&text) {
        redacted.push_str(&text[cursor..a]);
        redacted.push_str(REDACTED);
        cursor = b;
    }
    redacted.push_str(&text[cursor..]);

    let collapsed = collapse_home(&redacted, home);
    let flat: String = collapsed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_ERROR_MESSAGE_CHARS {
        let mut cut: String = flat.chars().take(MAX_ERROR_MESSAGE_CHARS - 3).collect();
        cut.push_str("...");
        cut
    } else {
        flat
    }
}

/// Summed persistent index cache lookups over the recorded indexing passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexCacheTotals {
    pub passes: u64,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
}

/// Operational metrics read back from the audit database (plan 4 step 4.8). `None`
/// means the table is absent (database written before these metrics existed).
#[derive(Debug, Clone, Default)]
pub struct AuditMetrics {
    /// `(tool, duration_us)` per timed call.
    pub tool_latencies_us: Option<Vec<(String, u64)>>,
    pub index_cache: Option<IndexCacheTotals>,
    /// `(process, workspace)` per recorded start.
    pub process_starts: Option<Vec<(String, String)>>,
}

/// Robust cryptographic multi-process AuditLogger backed by SQLite in WAL mode per CLAUDE.md Commandment 7.
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
    ///
    /// Every row, with or without an error detail, is written as v2 with the v2 hash
    /// input byte for byte, so an older binary (a stale `meshd`, a rollback) still
    /// verifies the whole chain. The sanitized `error_detail` is bound separately by
    /// `error_detail_sig` (see [`Self::error_detail_sig`]): an older binary neither
    /// sees nor verifies the detail, a current one rejects a forged or unsigned one.
    pub const CHAIN_VERSION: i64 = 2;

    /// `SHA-256(entry_hash || error_detail)`: binds a row's stored error text to the
    /// row it belongs to (and, through `entry_hash`, to the whole chain before it).
    pub fn error_detail_sig(entry_hash: &str, error_detail: &str) -> String {
        Self::compute_sha256(format!("{entry_hash}{error_detail}").as_bytes())
    }

    /// Environment variable overriding [`Self::default_db_path`]: benchmarks, test
    /// harnesses and pilot runs point it at their own file so they never mix their
    /// calls into the user's real audit trail (7.0.0 pilot feedback: 314 test calls
    /// and scale-bench files dominated `mesh-mcp stats` on a developer machine).
    pub const DB_PATH_ENV: &'static str = "MESH_AUDIT_DB";

    /// `$MESH_AUDIT_DB` when set and non-empty, else `~/.cache/mesh-mcp/audit.db`.
    pub fn default_db_path() -> PathBuf {
        match std::env::var_os(Self::DB_PATH_ENV) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => crate::paths::mesh_cache_dir().join("audit.db"),
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
                 chain_version INTEGER NOT NULL DEFAULT 1,
                 error_detail TEXT,
                 error_detail_sig TEXT
             );
             -- `entry_seq` is INTEGER PRIMARY KEY, i.e. the rowid: it is already the
             -- table's B-tree key. The secondary index earlier versions created on it
             -- only doubled every insert's write cost.
             DROP INDEX IF EXISTS idx_audit_seq;
             -- Operational metrics (plan 4 step 4.8), deliberately outside the hash chain.
             CREATE TABLE IF NOT EXISTS tool_call_metrics (
                 entry_seq INTEGER PRIMARY KEY,
                 duration_us INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS index_cache_metrics (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT NOT NULL,
                 hits INTEGER NOT NULL,
                 misses INTEGER NOT NULL,
                 errors INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS process_starts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 timestamp TEXT NOT NULL,
                 process TEXT NOT NULL,
                 pid INTEGER NOT NULL,
                 workspace TEXT NOT NULL
             );",
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

        // Same migration for `error_detail` and `error_detail_sig` (7.1.8): rows written
        // before them keep NULL in both and keep verifying under their chain version.
        for column in ["error_detail", "error_detail_sig"] {
            if Self::has_column(&conn, column)? {
                continue;
            }
            if let Err(err) = conn.execute(
                &format!("ALTER TABLE audit_entries ADD COLUMN {column} TEXT"),
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

    fn has_column(conn: &Connection, column: &str) -> Result<bool, AuditError> {
        Ok(conn
            .prepare("PRAGMA table_info(audit_entries)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(|name| name.ok())
            .any(|name| name == column))
    }

    /// The `SELECT` for every audit row. A database written before `error_detail`
    /// existed (opened read-only, so not migrated) yields `NULL` for it.
    fn select_entries_sql(conn: &Connection) -> Result<String, AuditError> {
        let error_columns = if Self::has_column(conn, "error_detail")?
            && Self::has_column(conn, "error_detail_sig")?
        {
            "error_detail, error_detail_sig"
        } else if Self::has_column(conn, "error_detail")? {
            "error_detail, NULL"
        } else {
            "NULL, NULL"
        };
        Ok(format!(
            "SELECT entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                    args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                    chain_version, {error_columns}
             FROM audit_entries ORDER BY entry_seq ASC"
        ))
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
        self.record_entry_with_error(
            session_id,
            trace_id,
            tool,
            args_json,
            status,
            files_accessed,
            secrets_redacted_count,
            None,
        )
    }

    /// [`Self::record_entry`] with the sanitized error of a failed call. The row keeps
    /// the v2 hash input; the detail is stored with its own `error_detail_sig`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_entry_with_error(
        &self,
        session_id: &str,
        trace_id: Option<&str>,
        tool: &str,
        args_json: &str,
        status: &str,
        files_accessed: Vec<String>,
        secrets_redacted_count: usize,
        error: Option<&ErrorDetail>,
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
        let error_detail = error.map(ErrorDetail::to_stored);
        let chain_version = Self::CHAIN_VERSION;
        let hash_input = format!(
            "{prev_hash}{timestamp}{session_id}{tool}{args_digest}{status}{files_json}{secrets_redacted_count}"
        );
        let entry_hash = Self::compute_sha256(hash_input.as_bytes());
        let error_detail_sig = error_detail
            .as_deref()
            .map(|d| Self::error_detail_sig(&entry_hash, d));

        tx.prepare_cached(
            "INSERT INTO audit_entries (
                entry_seq, prev_hash, timestamp, session_id, trace_id, tool,
                args_digest, status, files_accessed, secrets_redacted_count, entry_hash,
                chain_version, error_detail, error_detail_sig
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            chain_version,
            error_detail,
            error_detail_sig,
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
            chain_version,
            error_detail,
            error_detail_sig,
        })
    }

    /// Verifies the cryptographic integrity of the SQLite audit database.
    pub fn verify_db(path: &Path) -> Result<bool, AuditError> {
        if !path.exists() {
            return Ok(true);
        }

        let conn = Connection::open(path)?;
        let mut stmt = conn.prepare(&Self::select_entries_sql(&conn)?)?;

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
                    error_detail: row.get(12)?,
                    error_detail_sig: row.get(13)?,
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
                // v2 and any later version: the v2 input (the error detail is not part
                // of it, see `error_detail_sig`).
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

            // The error detail is outside the hash input (so older binaries keep
            // verifying) and bound by its own signature instead. A detail with no
            // valid signature, or a signature with no detail, is tampering: this
            // applies to every row, including v1 / v2 rows that predate the column.
            match (&entry.error_detail, &entry.error_detail_sig) {
                (None, None) => {}
                (Some(detail), Some(sig)) => {
                    let expected = Self::error_detail_sig(&entry.entry_hash, detail);
                    if &expected != sig {
                        return Err(AuditError::BrokenChain(
                            entry.entry_seq,
                            format!("error_detail_sig {expected}"),
                            format!("error_detail_sig {sig}"),
                        ));
                    }
                }
                (Some(_), None) => {
                    return Err(AuditError::BrokenChain(
                        entry.entry_seq,
                        "signed error_detail".to_string(),
                        "unsigned error_detail".to_string(),
                    ))
                }
                (None, Some(_)) => {
                    return Err(AuditError::BrokenChain(
                        entry.entry_seq,
                        "no error_detail_sig".to_string(),
                        "error_detail_sig without error_detail".to_string(),
                    ))
                }
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
        let mut stmt = conn.prepare(&Self::select_entries_sql(&conn)?)?;

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
                error_detail: row.get(12)?,
                error_detail_sig: row.get(13)?,
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

    /// Records how long the tool call audited as `entry_seq` took, in microseconds
    /// (plan 4 step 4.8, read back by `mesh-mcp stats`).
    ///
    /// Operational metrics live in their own tables, outside the SHA-256 chain: they are
    /// measurements, not audit evidence, and folding them into the chain would change the
    /// hash formula of every future row. `stats` reads them; `verify_db` ignores them.
    pub fn record_tool_latency(&self, entry_seq: u64, duration_us: u64) -> Result<(), AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO tool_call_metrics (entry_seq, duration_us) VALUES (?1, ?2)",
        )?
        .execute(params![entry_seq, duration_us as i64])?;
        Ok(())
    }

    /// Records the persistent index cache lookups of one indexing pass (the delta of
    /// `PersistentIndexCache::stats()` over that pass). Those counters are otherwise
    /// in-memory only and die with the process.
    pub fn record_index_cache_pass(
        &self,
        hits: u64,
        misses: u64,
        errors: u64,
    ) -> Result<(), AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;
        conn.prepare_cached(
            "INSERT INTO index_cache_metrics (timestamp, hits, misses, errors)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            chrono_fallback_utc_now(),
            hits as i64,
            misses as i64,
            errors as i64
        ])?;
        Ok(())
    }

    /// Records that a process holding an `AppState` started (`process` is its executable
    /// name, e.g. `meshd`; `workspace` identifies the roots it serves). `stats` derives
    /// daemon restarts from these rows.
    pub fn record_process_start(&self, process: &str, workspace: &str) -> Result<(), AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;
        conn.prepare_cached(
            "INSERT INTO process_starts (timestamp, process, pid, workspace)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            chrono_fallback_utc_now(),
            process,
            i64::from(std::process::id()),
            workspace
        ])?;
        Ok(())
    }

    /// Reads the operational metrics recorded in `path` since `since_epoch_secs`.
    /// Each field is `None` when its table does not exist, i.e. the database was
    /// written by a version that did not record that metric: "not recorded" must
    /// never be reported as zero.
    pub fn read_metrics(
        path: &Path,
        since_epoch_secs: Option<f64>,
    ) -> Result<AuditMetrics, AuditError> {
        Self::read_metrics_for_session(path, since_epoch_secs, None)
    }

    /// [`Self::read_metrics`], with tool latencies restricted to calls audited
    /// under `session` when given. Index cache passes and process starts are not
    /// tied to a session and are returned unfiltered.
    pub fn read_metrics_for_session(
        path: &Path,
        since_epoch_secs: Option<f64>,
        session: Option<&str>,
    ) -> Result<AuditMetrics, AuditError> {
        let mut metrics = AuditMetrics::default();
        if !path.exists() {
            return Ok(metrics);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let has_table = |name: &str| -> Result<bool, AuditError> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };
        let in_window = |ts: &str| {
            since_epoch_secs.is_none_or(|cutoff| parse_timestamp_to_epoch_secs(ts) >= cutoff)
        };

        if has_table("tool_call_metrics")? {
            let mut stmt = conn.prepare(
                "SELECT e.tool, m.duration_us, e.timestamp, e.session_id
                 FROM tool_call_metrics m JOIN audit_entries e ON e.entry_seq = m.entry_seq
                 ORDER BY m.entry_seq ASC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (tool, duration_us, ts, row_session) = row?;
                if in_window(&ts) && session.is_none_or(|s| s == row_session) {
                    out.push((tool, duration_us.max(0) as u64));
                }
            }
            metrics.tool_latencies_us = Some(out);
        }

        if has_table("index_cache_metrics")? {
            let mut stmt =
                conn.prepare("SELECT timestamp, hits, misses, errors FROM index_cache_metrics")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            let mut totals = IndexCacheTotals::default();
            for row in rows {
                let (ts, hits, misses, errors) = row?;
                if in_window(&ts) {
                    totals.passes += 1;
                    totals.hits += hits.max(0) as u64;
                    totals.misses += misses.max(0) as u64;
                    totals.errors += errors.max(0) as u64;
                }
            }
            metrics.index_cache = Some(totals);
        }

        if has_table("process_starts")? {
            let mut stmt = conn
                .prepare("SELECT timestamp, process, workspace FROM process_starts ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (ts, process, workspace) = row?;
                if in_window(&ts) {
                    out.push((process, workspace));
                }
            }
            metrics.process_starts = Some(out);
        }

        Ok(metrics)
    }

    /// Exports all audit entries to a JSON Lines (JSONL) flat file for compliance tooling
    pub fn export_to_jsonl(&self, dest: &Path) -> Result<usize, AuditError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| std::io::Error::other("AuditLogger db mutex poisoned"))?;

        let mut stmt = conn.prepare(&Self::select_entries_sql(&conn)?)?;

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
                error_detail: row.get(12)?,
                error_detail_sig: row.get(13)?,
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
    format_epoch_iso(now.duration_since(UNIX_EPOCH).unwrap_or_default())
}

/// An audit row's timestamp as ISO 8601 UTC, whichever of the two formats it was
/// written in (early rows hold `<epoch-secs>.<millis>Z`, later ones ISO 8601).
pub fn normalize_audit_timestamp(ts: &str) -> String {
    // Rounded to the millisecond both formats store: `from_secs_f64` alone
    // truncated `…21.564` to `…21.563`.
    let millis = (parse_timestamp_to_epoch_secs(ts).max(0.0) * 1000.0).round() as u64;
    format_epoch_iso(std::time::Duration::from_millis(millis))
}

fn format_epoch_iso(duration: std::time::Duration) -> String {
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

/// Seconds since the Unix epoch of an audit timestamp in either stored format
/// (see [`normalize_audit_timestamp`]); 0 when unparseable.
pub fn parse_timestamp_to_epoch_secs(ts: &str) -> f64 {
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

    /// Plan 4 step 4.8: a database written before the metrics tables existed reports
    /// every metric as "not recorded" (`None`), never as zero.
    #[test]
    fn read_metrics_on_legacy_db_reports_not_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("legacy.db");
        let conn = Connection::open(&db).expect("open");
        conn.execute_batch(
            "CREATE TABLE audit_entries (entry_seq INTEGER PRIMARY KEY, prev_hash TEXT NOT NULL,
             timestamp TEXT NOT NULL, session_id TEXT NOT NULL, trace_id TEXT, tool TEXT NOT NULL,
             args_digest TEXT NOT NULL, status TEXT NOT NULL, files_accessed TEXT NOT NULL,
             secrets_redacted_count INTEGER NOT NULL, entry_hash TEXT NOT NULL);",
        )
        .expect("legacy schema");
        drop(conn);
        let m = AuditLogger::read_metrics(&db, None).expect("read");
        assert!(m.tool_latencies_us.is_none());
        assert!(m.index_cache.is_none());
        assert!(m.process_starts.is_none());
    }

    /// Metrics rows are outside the hash chain: recording them must not break it.
    #[test]
    fn metrics_rows_do_not_affect_chain_verification() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("audit.db");
        let logger = AuditLogger::new(Some(db.clone())).expect("logger");
        let e = logger
            .record_entry("s", None, "smart_search", "{}", "SUCCESS", vec![], 0)
            .expect("entry");
        logger
            .record_tool_latency(e.entry_seq, 1234)
            .expect("latency");
        logger.record_index_cache_pass(1, 2, 0).expect("cache");
        logger.record_process_start("meshd", "/ws").expect("start");
        assert!(AuditLogger::verify_db(&db).expect("verify"));

        let m = AuditLogger::read_metrics(&db, None).expect("read");
        assert_eq!(
            m.tool_latencies_us,
            Some(vec![("smart_search".to_string(), 1234)])
        );
        // A window starting in the future excludes every row.
        let future = AuditLogger::read_metrics(&db, Some(f64::MAX)).expect("read");
        assert_eq!(future.tool_latencies_us, Some(vec![]));
        assert_eq!(future.index_cache.map(|c| c.passes), Some(0));
        assert_eq!(future.process_starts, Some(vec![]));
    }

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

    /// What a 7.1.7 binary does: v1 / v2 formulas over the original twelve columns,
    /// knowing nothing of `error_detail` / `error_detail_sig`.
    fn old_binary_verifies(path: &Path) -> bool {
        let conn = Connection::open(path).expect("open");
        let mut stmt = conn
            .prepare(
                "SELECT entry_seq, prev_hash, timestamp, session_id, tool, args_digest, status,
                        files_accessed, secrets_redacted_count, entry_hash, chain_version
                 FROM audit_entries ORDER BY entry_seq ASC",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, i64>(10)?,
                ))
            })
            .expect("rows");
        let mut prev = AuditLogger::GENESIS_HASH.to_string();
        for row in rows {
            let (p, ts, sess, tool, dig, status, files, red, hash, ver) = row.expect("row");
            if p != prev {
                return false;
            }
            let input = if ver == 1 {
                format!("{p}{ts}{sess}{tool}{dig}")
            } else {
                format!("{p}{ts}{sess}{tool}{dig}{status}{files}{red}")
            };
            if AuditLogger::compute_sha256(input.as_bytes()) != hash {
                return false;
            }
            prev = hash;
        }
        true
    }

    /// A legacy v1 database (epoch-style timestamp, no `error_detail` columns) opened by
    /// this binary is migrated in place; new rows chain onto it as v2, the whole mixed
    /// chain verifies (also under the 7.1.7 formulas), and a forged, unsigned or
    /// re-signed-wrong detail breaks the current verifier.
    #[test]
    fn test_mixed_legacy_and_new_chain_verifies_and_detects_error_tampering() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("mixed.db");

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
        .expect("legacy schema");
        let digest = AuditLogger::compute_sha256(b"{}");
        let legacy_hash = AuditLogger::compute_sha256(
            format!(
                "{}{}{}{}{}",
                AuditLogger::GENESIS_HASH,
                "1789974857.459Z",
                "old",
                "smart_search",
                digest
            )
            .as_bytes(),
        );
        conn.execute(
            "INSERT INTO audit_entries VALUES (0, ?1, '1789974857.459Z', 'old', NULL,
                'smart_search', ?2, 'ERROR', '[]', 0, ?3, 1)",
            params![AuditLogger::GENESIS_HASH, digest, legacy_hash],
        )
        .expect("legacy row");
        drop(conn);

        // Read-only access to the not yet migrated database works.
        let before = AuditLogger::read_entries(&db_file, None).expect("read legacy");
        assert_eq!(before.len(), 1);
        assert!(before[0].error_detail.is_none());

        let logger = AuditLogger::new(Some(db_file.clone())).expect("migrating open");
        let ok = logger
            .record_entry("s", None, "smart_search", "{}", "SUCCESS", vec![], 0)
            .expect("v2 row");
        assert_eq!(ok.chain_version, AuditLogger::CHAIN_VERSION);
        assert!(ok.error_detail.is_none());
        let detail = ErrorDetail::sanitized("invalid_params", -32602, "Path not found: src/x");
        let err = logger
            .record_entry_with_error(
                "s",
                None,
                "smart_search",
                "{\"query\":\"secret-arg\"}",
                "ERROR",
                vec![],
                0,
                Some(&detail),
            )
            .expect("error row");
        assert_eq!(err.chain_version, AuditLogger::CHAIN_VERSION);
        assert!(err.error_detail_sig.is_some());
        assert!(
            ISO_TS.is_match_prefix(&err.timestamp),
            "new rows are ISO 8601: {}",
            err.timestamp
        );
        drop(logger);

        assert!(AuditLogger::verify_db(&db_file).expect("mixed chain verifies"));
        let rows = AuditLogger::read_entries(&db_file, None).expect("read");
        assert_eq!(rows.len(), 3);
        let stored = rows[2].error_detail.as_deref().expect("stored detail");
        assert_eq!(ErrorDetail::from_stored(stored), Some(detail));
        assert!(
            !stored.contains("secret-arg"),
            "raw arguments are never stored"
        );
        // The legacy epoch-style stamp is tolerated and normalised on read, not rewritten.
        assert_eq!(rows[0].timestamp, "1789974857.459Z");
        assert_eq!(
            normalize_audit_timestamp(&rows[0].timestamp),
            "2026-09-21T07:14:17.459Z"
        );

        // An older verifier passes over the same database: the detail is invisible to it.
        assert!(old_binary_verifies(&db_file), "7.1.7 formulas still verify");

        let forged = r#"{"class":"x","code":0,"message":"edited"}"#;
        let run = |sql: &str, args: &[&dyn rusqlite::ToSql]| {
            let conn = Connection::open(&db_file).expect("open");
            conn.execute(sql, args).expect("tamper");
        };
        // (a) edited detail on a signed row.
        run(
            "UPDATE audit_entries SET error_detail = ?1 WHERE entry_seq = 2",
            &[&forged],
        );
        assert!(AuditLogger::verify_db(&db_file).is_err(), "edited detail");
        assert!(old_binary_verifies(&db_file), "old verifier cannot see it");
        // (b) edited signature (restore the detail first).
        run(
            "UPDATE audit_entries SET error_detail = ?1 WHERE entry_seq = 2",
            &[&stored],
        );
        assert!(AuditLogger::verify_db(&db_file).expect("restored"));
        run(
            "UPDATE audit_entries SET error_detail_sig = ?1 WHERE entry_seq = 2",
            &[&"0".repeat(64)],
        );
        assert!(AuditLogger::verify_db(&db_file).is_err(), "edited sig");
        // (c) forged detail on a legacy v1 row and on a v2 row, signed or not.
        run(
            "UPDATE audit_entries SET error_detail_sig = ?1 WHERE entry_seq = 2",
            &[&rows[2].error_detail_sig],
        );
        assert!(AuditLogger::verify_db(&db_file).expect("restored"));
        for seq in [0i64, 1] {
            run(
                "UPDATE audit_entries SET error_detail = ?1 WHERE entry_seq = ?2",
                &[&forged, &seq],
            );
            assert!(
                AuditLogger::verify_db(&db_file).is_err(),
                "unsigned on {seq}"
            );
            run(
                "UPDATE audit_entries SET error_detail_sig = ?1 WHERE entry_seq = ?2",
                &[&"1".repeat(64), &seq],
            );
            assert!(
                AuditLogger::verify_db(&db_file).is_err(),
                "bad sig on {seq}"
            );
            run(
                "UPDATE audit_entries SET error_detail = NULL, error_detail_sig = NULL
                 WHERE entry_seq = ?1",
                &[&seq],
            );
            assert!(AuditLogger::verify_db(&db_file).expect("restored"));
        }
        // (d) a signature with no detail.
        run(
            "UPDATE audit_entries SET error_detail_sig = ?1 WHERE entry_seq = 1",
            &[&"2".repeat(64)],
        );
        assert!(AuditLogger::verify_db(&db_file).is_err(), "orphan sig");
    }

    struct IsoShape;
    const ISO_TS: IsoShape = IsoShape;
    impl IsoShape {
        /// `YYYY-MM-DDTHH:MM:SS.mmmZ`
        fn is_match_prefix(&self, ts: &str) -> bool {
            let b = ts.as_bytes();
            b.len() == 24
                && b[4] == b'-'
                && b[7] == b'-'
                && b[10] == b'T'
                && b[13] == b':'
                && b[16] == b':'
                && b[19] == b'.'
                && b[23] == b'Z'
        }
    }

    #[test]
    fn sanitizer_covers_urls_jwts_serde_values_and_prose() {
        let s = |m: &str| sanitize_error_message(m, None);
        let hidden = "[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]";

        // URL userinfo, in a connection string and in a git error.
        let m = s("connect postgres://app:hunter2pw@db.internal:5432/orders failed");
        assert_eq!(
            m,
            format!("connect postgres://{hidden}@db.internal:5432/orders failed")
        );
        let m = s("fatal: unable to access 'https://bob:ghp_abcdef@github.com/o/r.git/'");
        assert!(!m.contains("bob") && !m.contains("ghp_abcdef"), "{m}");
        assert!(m.contains("@github.com/o/r.git"), "{m}");

        let m = s("Path not found: /w/https:/u:pw12345@host.example/x");
        assert!(
            !m.contains("pw12345") && m.contains("@host.example/x"),
            "{m}"
        );
        assert_eq!(
            s("contact dev@example.com now"),
            "contact dev@example.com now"
        );

        // A JWT, whole.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.c2lnbmF0dXJlLXBhcnQ";
        let m = s(&format!("rejected {jwt} today"));
        assert_eq!(m, format!("rejected {hidden} today"));
        // Base64url key material with `-` and `_`.
        let m = s("key Ab3dEf-Gh1jKl_Mn2pQr-St4uVw_Xy5zAb6cDe7 end");
        assert!(!m.contains("Ab3dEf"), "{m}");

        // serde-style errors keep the schema, never the user's value.
        let m = s("unknown variant `rm -rf /secret`, expected one of `a`, `b`");
        assert_eq!(m, "unknown variant <value>, expected one of `a`, `b`");
        let m = s("unknown field `my_password_field`, expected `query`");
        assert!(!m.contains("my_password_field"), "{m}");
        let m = s("invalid value: string \"top, expected leak\", expected a number");
        assert_eq!(m, "invalid value: string <value>, expected a number");
        let m = s("invalid type: null, expected a string");
        assert_eq!(m, "invalid type: null, expected a string");
        let m = s("invalid type: string \"unterminated");
        assert_eq!(m, "invalid type: string <value>");

        // Ordinary prose after `basic` / `bearer` is kept.
        assert_eq!(s("basic usage is documented"), "basic usage is documented");
        assert_eq!(s("the bearer of bad news"), "the bearer of bad news");
        let m = s("Authorization: Basic dXNlcjpwYXNzd29yZDE=");
        assert!(!m.contains("dXNlcjpw"), "{m}");
    }

    #[test]
    fn error_messages_are_redacted_collapsed_and_truncated() {
        let home = Some("/Users/alice");
        let m = sanitize_error_message(
            "Path not found: /Users/alice/work/repo/src/a.rs (token=abc123def, password: \"hunter2\")",
            home,
        );
        assert!(m.contains("~/work/repo/src/a.rs"), "{m}");
        assert!(
            !m.contains("alice") && !m.contains("abc123def") && !m.contains("hunter2"),
            "{m}"
        );
        assert!(
            m.contains("[REDACTED_SECRET: USE_ENV_OR_LOCAL_FALLBACK]"),
            "{m}"
        );

        // Another user's home and a Windows home collapse too.
        let m = sanitize_error_message("a /home/bob/x b C:\\Users\\carol\\y c", None);
        assert_eq!(m, "a ~/x b ~\\y c");

        // Bearer tokens, prefixed tokens and long mixed blobs.
        let m = sanitize_error_message(
            "Authorization: Bearer abcdef0123456789 ghp_0123456789abcdefghij 0123456789abcdef0123456789abcdef0123",
            None,
        );
        assert!(
            !m.contains("abcdef0123456789") && !m.contains("ghp_0123"),
            "{m}"
        );

        // serde echoes of offending string values are cut.
        let m = sanitize_error_message(
            "invalid type: string \"my-private-value\", expected u32",
            None,
        );
        assert!(!m.contains("my-private-value"), "{m}");
        assert_eq!(m, "invalid type: string <value>, expected u32");

        // Truncation to 200 chars, newlines flattened.
        let long = format!("line1\nline2 {}", "word ".repeat(100));
        let m = sanitize_error_message(&long, None);
        assert_eq!(m.chars().count(), MAX_ERROR_MESSAGE_CHARS);
        assert!(m.ends_with("...") && !m.contains('\n'));

        // A long hyphenated directory name is not a token.
        let path =
            "x/-Users-victoragahi-Developer-causalmesh/c758ea9a-eb19-4085-b418-b0d8bdef0e98/s";
        assert_eq!(sanitize_error_message(path, None), path);

        // Plain text is left alone.
        assert_eq!(
            sanitize_error_message("Scope escapes the workspace roots", None),
            "Scope escapes the workspace roots"
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

    #[test]
    fn normalize_audit_timestamp_handles_both_stored_formats() {
        assert_eq!(
            normalize_audit_timestamp("1700000000.000Z"),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            normalize_audit_timestamp("2026-09-28T08:32:21.564Z"),
            "2026-09-28T08:32:21.564Z"
        );
    }

    /// `MESH_AUDIT_DB` redirects the default database (benches, pilots, tests).
    #[test]
    fn default_db_path_honors_the_env_override() {
        let prev = std::env::var_os(AuditLogger::DB_PATH_ENV);
        std::env::set_var(AuditLogger::DB_PATH_ENV, "/tmp/pilot-run-1.db");
        assert_eq!(
            AuditLogger::default_db_path(),
            PathBuf::from("/tmp/pilot-run-1.db")
        );
        std::env::set_var(AuditLogger::DB_PATH_ENV, "");
        assert!(AuditLogger::default_db_path().ends_with("audit.db"));
        match prev {
            Some(v) => std::env::set_var(AuditLogger::DB_PATH_ENV, v),
            None => std::env::remove_var(AuditLogger::DB_PATH_ENV),
        }
    }
}
