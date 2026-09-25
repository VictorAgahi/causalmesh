//! Persistent, content-hash-keyed cache of parsed per-file extraction results (RFC-001 P2
//! step 3.2), backed by SQLite in WAL mode at `~/.cache/mesh-mcp/index-cache.db` (Commandment
//! 7's audit db convention, mode `0600`, same directory).
//!
//! The cache stores whatever a caller serializes for one file (`mesh-server`'s
//! `WorkspaceIndexer` uses it for `mesh_parsers::FileIndex`, the AST-decapitated,
//! tree-sitter-derived per-file fragment) under a key folding together every input that
//! fragment depends on: file path, content hash, `RepoId` and a caller-supplied extraction
//! config fingerprint. An unchanged file rescanned under an unchanged config on a later cold
//! start is then a single indexed SQLite lookup instead of a full tree-sitter re-parse.
//!
//! Deliberately does *not* know about `NodeId`: `NodeId`s are assigned sequentially while
//! folding files into the `ContractGraph` in sorted crawl order (idempotence invariant I1),
//! not derived from content, so they are never stable across an incremental add/remove
//! elsewhere in the workspace. Caching the pre-numbering per-file fragment sidesteps that
//! entirely — global `NodeId`s are still (re-)assigned fresh on every fold, cache hit or not.

use ring::digest::{Context, SHA256};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

use crate::types::RepoId;

#[derive(Debug, Error)]
pub enum IndexCacheError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// One cache row's (key, serialized payload) pair, as written by `put_batch` and produced by
/// a caller's own cache-miss extraction.
pub type CacheEntry = ([u8; 32], Vec<u8>);

/// Bumped whenever the cached payload's meaning changes (a new field callers now expect, a
/// different serialization). Folded into every cache key, so a version bump silently orphans
/// every old row (they simply never match again) instead of requiring an explicit migration
/// or a stored-and-checked version column.
const SCHEMA_VERSION: u8 = 1;

/// SHA-256 of `data`. Exposed so callers computing a `PersistentIndexCache::key_for` input
/// fingerprint (e.g. `mesh-server`'s per-scan extraction-config fingerprint) don't need `ring`
/// as a direct dependency of their own.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut ctx = Context::new(&SHA256);
    ctx.update(data);
    let digest = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Content-hash-keyed cache of one blob per (path, content hash, repo, config) tuple.
///
/// `conn` is a single `Mutex<Connection>`, so every `get()` from every parallel scan thread
/// serializes on one lock — deliberately not a connection pool. A read this cheap (one indexed
/// lookup) is still far faster serialized than a tree-sitter parse run in parallel, which is
/// the comparison that matters for step 3.2's goal; sharding reads across a connection pool
/// would only be worth the added complexity if lock contention itself became the bottleneck,
/// which the measured boot-time wins (`docs/quality.md`) show it currently is not.
pub struct PersistentIndexCache {
    conn: Mutex<Connection>,
}

impl PersistentIndexCache {
    pub fn default_db_path() -> PathBuf {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            PathBuf::from(home)
                .join(".cache")
                .join("mesh-mcp")
                .join("index-cache.db")
        } else {
            std::env::temp_dir().join("mesh-mcp").join("index-cache.db")
        }
    }

    /// Opens (creating if absent) the cache database at `path`, or `default_db_path()` when
    /// `None`. Deliberately duplicates (rather than shares) `AuditLogger::new`'s WAL/permissions
    /// setup (Commandment 7): the two are independent databases with independent lifecycles —
    /// this one is a disposable performance cache safe to delete any time, the audit log is a
    /// cryptographically chained record that must never be — and factoring the setup into a
    /// shared helper now would couple their futures (e.g. a WAL-corruption workaround needed by
    /// one but not the other) for a few dozen lines saved today.
    pub fn open(path: Option<PathBuf>) -> Result<Self, IndexCacheError> {
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
             CREATE TABLE IF NOT EXISTS file_index_cache (
                 cache_key BLOB PRIMARY KEY,
                 payload BLOB NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;

        if db_path.to_string_lossy() != ":memory:" {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
            }
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self, IndexCacheError> {
        Self::open(Some(PathBuf::from(":memory:")))
    }

    /// Cache key for one file's extraction: every input the caller's extraction depends on
    /// (path, content hash, repo id, a config fingerprint) folded together with
    /// `SCHEMA_VERSION`. Two files with identical bytes at different paths, or the same file
    /// re-scanned under a changed config, get different keys — this cache never conflates
    /// them.
    pub fn key_for(
        path: &Path,
        content_hash: &[u8; 32],
        repo_id: RepoId,
        config_fingerprint: &[u8; 32],
    ) -> [u8; 32] {
        let mut ctx = Context::new(&SHA256);
        ctx.update(&[SCHEMA_VERSION]);
        ctx.update(path.to_string_lossy().as_bytes());
        ctx.update(content_hash);
        ctx.update(&repo_id.to_le_bytes());
        ctx.update(config_fingerprint);
        let digest = ctx.finish();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        out
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match conn
            .query_row(
                "SELECT payload FROM file_index_cache WHERE cache_key = ?1",
                params![key.as_slice()],
                |row| row.get(0),
            )
            .optional()
        {
            Ok(payload) => payload,
            // A genuine SQLite error (locked past `busy_timeout`, corrupted db, ...) is not
            // the same as a normal cache miss: `.optional()` already turned "no rows" into
            // `Ok(None)`, so anything reaching here is worth surfacing — silently returning
            // `None` on every lookup forever, indistinguishable from a merely cold cache,
            // would otherwise hide a real regression (see `put_batch`, which already logs).
            Err(e) => {
                tracing::warn!(target: "mesh::index_cache", "Cache lookup failed: {e}");
                None
            }
        }
    }

    /// One transaction for every cache miss this scan produced. Called once after a full
    /// parallel scan pass completes, never per file: many small transactions against one WAL
    /// connection would serialize scan threads against fsync latency, defeating the point of
    /// running extraction on the Rayon pool in the first place.
    pub fn put_batch(&self, entries: &[CacheEntry]) {
        if entries.is_empty() {
            return;
        }
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                tracing::warn!(
                    target: "mesh::index_cache",
                    "Failed to open cache write transaction: {e}"
                );
                return;
            }
        };
        for (key, payload) in entries {
            if let Err(e) = tx.execute(
                "INSERT OR REPLACE INTO file_index_cache (cache_key, payload, updated_at) \
                 VALUES (?1, ?2, ?3)",
                params![key.as_slice(), payload, now],
            ) {
                tracing::warn!(target: "mesh::index_cache", "Failed to write cache entry: {e}");
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!(target: "mesh::index_cache", "Failed to commit cache batch: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn miss_then_hit_after_put() {
        let cache = PersistentIndexCache::in_memory().expect("open");
        let hash = [7u8; 32];
        let fp = [9u8; 32];
        let key = PersistentIndexCache::key_for(Path::new("/a/b.rs"), &hash, 0, &fp);
        assert!(cache.get(&key).is_none());
        cache.put_batch(&[(key, b"payload".to_vec())]);
        assert_eq!(cache.get(&key), Some(b"payload".to_vec()));
    }

    #[test]
    fn sha256_is_deterministic_and_input_sensitive() {
        assert_eq!(sha256(b"x"), sha256(b"x"));
        assert_ne!(sha256(b"x"), sha256(b"y"));
    }

    #[test]
    fn different_path_same_content_hash_misses() {
        let hash = [1u8; 32];
        let fp = [2u8; 32];
        let key_a = PersistentIndexCache::key_for(Path::new("/a.rs"), &hash, 0, &fp);
        let key_b = PersistentIndexCache::key_for(Path::new("/b.rs"), &hash, 0, &fp);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn different_config_fingerprint_misses() {
        let hash = [1u8; 32];
        let key_a = PersistentIndexCache::key_for(Path::new("/a.rs"), &hash, 0, &[2u8; 32]);
        let key_b = PersistentIndexCache::key_for(Path::new("/a.rs"), &hash, 0, &[3u8; 32]);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn different_repo_id_misses() {
        let hash = [1u8; 32];
        let fp = [2u8; 32];
        let key_a = PersistentIndexCache::key_for(Path::new("/a.rs"), &hash, 0, &fp);
        let key_b = PersistentIndexCache::key_for(Path::new("/a.rs"), &hash, 1, &fp);
        assert_ne!(key_a, key_b);
    }
}
