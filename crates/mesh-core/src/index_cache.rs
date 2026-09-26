//! Persistent, content-hash-keyed cache of parsed per-file extraction results (RFC-001 P2
//! step 3.2), backed by SQLite in WAL mode, one database per workspace at
//! `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db` (plan 4 step 4.4; Commandment
//! 7's audit db convention, mode `0600`). `<workspace_id>` is the same
//! [`crate::socket::workspace_id`] that names the workspace's `meshd` socket.
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
//!
//! **Size bound (step 4.4).** Every database has a byte quota (`[cache] max_size_mb`, 2048 by
//! default). It is checked when the database is opened and after every [`put_batch`] that
//! wrote more than [`QUOTA_CHECK_MIN_WRITES`] entries (or left database + WAL over quota, a
//! stat-only check). Over quota, the least recently used
//! entries are deleted in batches of [`EVICTION_BATCH`] until the live data fits in 80 % of
//! the quota, then `PRAGMA incremental_vacuum` returns the freed pages to the filesystem.
//! Recency lives in a narrow side table (`file_index_access`) rather than in the payload row:
//! bumping a timestamp inside a ~1 KB payload row rewrites that row's whole page, so a warm
//! boot touching every entry would rewrite the whole database into the WAL.
//!
//! [`put_batch`]: PersistentIndexCache::put_batch

use ring::digest::{Context, SHA256};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Bumped whenever the cached payload's meaning or the on-disk layout changes. Folded into
/// every cache key (so an old row never matches again) and stored as `PRAGMA user_version`
/// (so a database written by another layout is dropped and recreated on open instead of
/// being migrated — this is a disposable cache).
/// v2: pattern / AsyncAPI / OpenAPI nodes carry their real declaration line instead
/// of a placeholder `1` (P2 step 3.4 review).
/// v3: one database per workspace, `file_index_access.last_accessed_at` LRU side table,
/// `auto_vacuum = INCREMENTAL` (plan 4 step 4.4).
const SCHEMA_VERSION: u8 = 3;

/// `[cache] max_size_mb` default: 2 GiB per workspace.
pub const DEFAULT_MAX_SIZE_MB: u64 = 2048;

/// A `put_batch` writing more entries than this re-checks the quota afterwards. Small
/// batches (a handful of edited files on an incremental reload) cannot move the size
/// meaningfully and skip the check.
pub const QUOTA_CHECK_MIN_WRITES: usize = 100;

/// Entries deleted per eviction transaction.
pub const EVICTION_BATCH: usize = 1000;

/// Eviction stops once live data fits in this share of the quota, so that a cache sitting
/// right at its quota does not evict on every write.
const EVICTION_TARGET_PERCENT: u64 = 80;

/// Upper bound on the WAL file left behind after a checkpoint. Without it a single large
/// transaction (a cold boot writing every entry at once) leaves a WAL as big as that
/// transaction on disk until the next `TRUNCATE` checkpoint.
const WAL_SIZE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

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

/// Counters since this cache was opened. `errors` counts every SQLite failure (including
/// `SQLITE_BUSY` past `busy_timeout`) that was swallowed to keep the cache optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
    pub evicted: u64,
}

/// What one [`PersistentIndexCache::enforce_quota`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuotaOutcome {
    /// Database + WAL bytes before the check.
    pub size_before: u64,
    /// Database + WAL bytes after the check (equal to `size_before` when under quota).
    pub size_after: u64,
    /// Entries deleted.
    pub evicted: u64,
}

struct Inner {
    conn: Connection,
    /// Keys served by `get` since the last `put_batch`, whose `last_accessed_at` is bumped
    /// in that batch's transaction instead of one write per read.
    touched: Vec<[u8; 32]>,
}

/// Content-hash-keyed cache of one blob per (path, content hash, repo, config) tuple.
///
/// `inner` is a single `Mutex`, so every `get()` from every parallel scan thread
/// serializes on one lock — deliberately not a connection pool. A read this cheap (one indexed
/// lookup) is still far faster serialized than a tree-sitter parse run in parallel, which is
/// the comparison that matters for step 3.2's goal; sharding reads across a connection pool
/// would only be worth the added complexity if lock contention itself became the bottleneck,
/// which the measured boot-time wins (`docs/quality.md`) show it currently is not.
pub struct PersistentIndexCache {
    inner: Mutex<Inner>,
    /// `None` for an in-memory database.
    db_path: Option<PathBuf>,
    max_size_bytes: u64,
    hits: AtomicU64,
    misses: AtomicU64,
    errors: AtomicU64,
    evicted: AtomicU64,
}

/// Unix time in milliseconds: `updated_at` and `last_accessed_at` use it so that two scans a
/// few milliseconds apart (tests, back-to-back reloads) still order correctly for LRU.
fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn wal_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

impl PersistentIndexCache {
    /// `~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db`.
    pub fn workspace_db_path(workspace_id: &str) -> PathBuf {
        crate::paths::mesh_cache_dir()
            .join("workspaces")
            .join(workspace_id)
            .join("index-cache.db")
    }

    /// The pre-4.4 machine-wide database (`~/.cache/mesh-mcp/index-cache.db`). Never opened
    /// any more; exposed so `doctor` can report and remove it.
    pub fn legacy_global_db_path() -> PathBuf {
        crate::paths::mesh_cache_dir().join("index-cache.db")
    }

    /// `[cache] max_size_mb` in bytes. `0` is treated as 1 MB: a zero quota would evict every
    /// entry right after writing it.
    pub fn quota_bytes_from_mb(max_size_mb: u64) -> u64 {
        max_size_mb.max(1).saturating_mul(1024 * 1024)
    }

    /// Opens the database of the workspace rooted at `base_dir` (see
    /// [`Self::workspace_db_path`]) with a quota of `max_size_mb`.
    pub fn open_for_workspace(base_dir: &Path, max_size_mb: u64) -> Result<Self, IndexCacheError> {
        let id = crate::socket::workspace_id(base_dir);
        Self::open(
            Self::workspace_db_path(&id),
            Self::quota_bytes_from_mb(max_size_mb),
        )
    }

    /// Opens (creating if absent) the cache database at `db_path` with a quota of
    /// `max_size_bytes`, then enforces the quota once. Deliberately duplicates (rather than
    /// shares) `AuditLogger::new`'s WAL/permissions setup (Commandment 7): the two are
    /// independent databases with independent lifecycles — this one is a disposable
    /// performance cache safe to delete any time, the audit log is a cryptographically chained
    /// record that must never be.
    pub fn open(db_path: PathBuf, max_size_bytes: u64) -> Result<Self, IndexCacheError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let conn = Connection::open(&db_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
        Self::from_connection(conn, Some(db_path), max_size_bytes)
    }

    /// In-memory database with the default quota (tests, and callers that want the cache's
    /// semantics without persistence).
    pub fn in_memory() -> Result<Self, IndexCacheError> {
        Self::from_connection(
            Connection::open_in_memory()?,
            None,
            Self::quota_bytes_from_mb(DEFAULT_MAX_SIZE_MB),
        )
    }

    fn from_connection(
        conn: Connection,
        db_path: Option<PathBuf>,
        max_size_bytes: u64,
    ) -> Result<Self, IndexCacheError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Must precede the first table creation to take effect without a VACUUM; a no-op on
        // a database that already has tables (handled by the version check below).
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;

        if db_path.is_some() {
            let current_mode: String = conn
                .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
                .unwrap_or_default();
            if !current_mode.eq_ignore_ascii_case("wal") {
                let _ = conn.query_row("PRAGMA journal_mode = WAL;", [], |_| Ok(()));
            }
        }
        conn.execute_batch(&format!(
            "PRAGMA synchronous = NORMAL; PRAGMA journal_size_limit = {WAL_SIZE_LIMIT_BYTES};"
        ))?;

        let user_version: i64 = conn.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
        if user_version != i64::from(SCHEMA_VERSION) {
            // A database from another layout (or a brand-new file): the cache is disposable,
            // so start over rather than migrate.
            conn.execute_batch(
                "DROP TABLE IF EXISTS file_index_access;
                 DROP TABLE IF EXISTS file_index_cache;",
            )?;
            let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum;", [], |r| r.get(0))?;
            if auto_vacuum != 2 {
                conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
            }
            conn.execute_batch(&format!(
                "CREATE TABLE file_index_cache (
                     cache_key BLOB PRIMARY KEY,
                     payload BLOB NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE file_index_access (
                     cache_key BLOB PRIMARY KEY,
                     last_accessed_at INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 CREATE INDEX file_index_access_lru
                     ON file_index_access (last_accessed_at, cache_key);
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))?;
        }

        let cache = Self {
            inner: Mutex::new(Inner {
                conn,
                touched: Vec::new(),
            }),
            db_path,
            max_size_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
        };
        cache.enforce_quota()?;
        Ok(cache)
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

    pub fn max_size_bytes(&self) -> u64 {
        self.max_size_bytes
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            evicted: self.evicted.load(Ordering::Relaxed),
        }
    }

    fn record_error(&self, what: &str, e: &rusqlite::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "mesh::index_cache", "{what}: {e}");
    }

    pub fn get(&self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner
            .conn
            .query_row(
                "SELECT payload FROM file_index_cache WHERE cache_key = ?1",
                params![key.as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
        {
            Ok(Some(payload)) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                inner.touched.push(*key);
                Some(payload)
            }
            Ok(None) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            // A genuine SQLite error (locked past `busy_timeout`, corrupted db, ...) is not
            // the same as a normal cache miss: `.optional()` already turned "no rows" into
            // `Ok(None)`, so anything reaching here is worth surfacing — silently returning
            // `None` on every lookup forever, indistinguishable from a merely cold cache,
            // would otherwise hide a real regression.
            Err(e) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                self.record_error("Cache lookup failed", &e);
                None
            }
        }
    }

    /// One transaction for every cache miss this scan produced, plus the batched
    /// `last_accessed_at` bump of every key `get` served since the previous call. Called once
    /// after a full parallel scan pass completes (also with no entries, to flush the access
    /// times), never per file: many small transactions against one WAL connection would
    /// serialize scan threads against fsync latency. Enforces the quota when more than
    /// [`QUOTA_CHECK_MIN_WRITES`] entries were written, or when the database + WAL already
    /// exceed it.
    pub fn put_batch(&self, entries: &[CacheEntry]) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if entries.is_empty() && inner.touched.is_empty() {
                return;
            }
            let touched = std::mem::take(&mut inner.touched);
            let now = unix_now_ms();
            let tx = match inner.conn.transaction() {
                Ok(tx) => tx,
                Err(e) => {
                    self.record_error("Failed to open cache write transaction", &e);
                    return;
                }
            };
            let mut failed = false;
            {
                let upsert_access = |tx: &rusqlite::Transaction, key: &[u8; 32]| {
                    tx.execute(
                        "INSERT INTO file_index_access (cache_key, last_accessed_at) \
                         VALUES (?1, ?2) \
                         ON CONFLICT(cache_key) DO UPDATE SET last_accessed_at = excluded.last_accessed_at",
                        params![key.as_slice(), now],
                    )
                };
                for (key, payload) in entries {
                    let res = tx
                        .execute(
                            "INSERT OR REPLACE INTO file_index_cache (cache_key, payload, updated_at) \
                             VALUES (?1, ?2, ?3)",
                            params![key.as_slice(), payload, now],
                        )
                        .and_then(|_| upsert_access(&tx, key));
                    if let Err(e) = res {
                        self.record_error("Failed to write cache entry", &e);
                        failed = true;
                    }
                }
                for key in &touched {
                    // `UPDATE`, not upsert: a key evicted between its `get` and this flush
                    // must not come back as an access row without a payload.
                    if let Err(e) = tx.execute(
                        "UPDATE file_index_access SET last_accessed_at = ?2 WHERE cache_key = ?1",
                        params![key.as_slice(), now],
                    ) {
                        self.record_error("Failed to record cache access", &e);
                        failed = true;
                    }
                }
            }
            if let Err(e) = tx.commit() {
                self.record_error("Failed to commit cache batch", &e);
                return;
            }
            if failed {
                return;
            }
        }
        // The plan's trigger (> 100 entries written) plus a stat-only size check: small
        // batches and access-time flushes also grow the WAL, and two `metadata` calls are
        // cheap enough to never let those drift past the quota until the next open.
        if entries.len() > QUOTA_CHECK_MIN_WRITES || self.size_bytes() > self.max_size_bytes {
            if let Err(e) = self.enforce_quota() {
                if let IndexCacheError::Sqlite(ref e) = e {
                    self.record_error("Cache quota enforcement failed", e);
                } else {
                    tracing::warn!(target: "mesh::index_cache", "Cache quota enforcement failed: {e}");
                }
            }
        }
    }

    /// Current footprint in bytes: database file plus its WAL (a file-backed cache), or
    /// allocated pages (in memory).
    pub fn size_bytes(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.size_locked(&inner.conn)
    }

    fn size_locked(&self, conn: &Connection) -> u64 {
        match &self.db_path {
            Some(p) => file_len(p) + file_len(&wal_path(p)),
            None => Self::pages(conn, "page_count").saturating_mul(Self::pages(conn, "page_size")),
        }
    }

    fn pages(conn: &Connection, pragma: &str) -> u64 {
        conn.query_row(&format!("PRAGMA {pragma};"), [], |r| r.get::<_, i64>(0))
            .map(|v| v.max(0) as u64)
            .unwrap_or(0)
    }

    /// Bytes of pages holding data (allocated pages minus the freelist).
    fn live_bytes(conn: &Connection) -> u64 {
        let used =
            Self::pages(conn, "page_count").saturating_sub(Self::pages(conn, "freelist_count"));
        used.saturating_mul(Self::pages(conn, "page_size"))
    }

    fn checkpoint(conn: &Connection) {
        // Best effort: a reader in another process can hold the WAL; the size check then
        // simply counts the WAL too.
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |_| Ok(()));
    }

    /// Brings the database under its quota: if database + WAL exceed it, deletes the least
    /// recently used entries in batches of [`EVICTION_BATCH`] until live data fits in 80 % of
    /// the quota, then releases the freed pages (`PRAGMA incremental_vacuum`) and truncates
    /// the WAL. Called by `open` and by `put_batch`; public for callers and tests.
    pub fn enforce_quota(&self) -> Result<QuotaOutcome, IndexCacheError> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = &inner.conn;
        if self.db_path.is_some() {
            Self::checkpoint(conn);
        }
        let size_before = self.size_locked(conn);
        if size_before <= self.max_size_bytes {
            return Ok(QuotaOutcome {
                size_before,
                size_after: size_before,
                evicted: 0,
            });
        }
        let target = self.max_size_bytes / 100 * EVICTION_TARGET_PERCENT;
        let mut evicted = 0u64;
        while Self::live_bytes(conn) > target {
            let keys: Vec<Vec<u8>> = {
                let mut stmt = conn.prepare_cached(
                    "SELECT cache_key FROM file_index_access \
                     ORDER BY last_accessed_at, cache_key LIMIT ?1",
                )?;
                let rows = stmt.query_map(params![EVICTION_BATCH as i64], |r| r.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            if keys.is_empty() {
                // Only payload rows without an access row can remain (never written by
                // this version): drop them oldest-first the same way.
                let removed = conn.execute(
                    "DELETE FROM file_index_cache WHERE cache_key IN (
                         SELECT cache_key FROM file_index_cache ORDER BY updated_at, cache_key LIMIT ?1)",
                    params![EVICTION_BATCH as i64],
                )?;
                if removed == 0 {
                    break;
                }
                evicted += removed as u64;
                continue;
            }
            let tx = conn.unchecked_transaction()?;
            for key in &keys {
                tx.execute(
                    "DELETE FROM file_index_cache WHERE cache_key = ?1",
                    params![key],
                )?;
                tx.execute(
                    "DELETE FROM file_index_access WHERE cache_key = ?1",
                    params![key],
                )?;
            }
            tx.commit()?;
            evicted += keys.len() as u64;
        }
        {
            // Each `sqlite3_step` of `incremental_vacuum` frees one page, so it must be stepped
            // to completion (`execute_batch` would free a single page).
            let mut stmt = conn.prepare("PRAGMA incremental_vacuum;")?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
        }
        if self.db_path.is_some() {
            Self::checkpoint(conn);
        }
        let size_after = self.size_locked(conn);
        self.evicted.fetch_add(evicted, Ordering::Relaxed);
        tracing::info!(
            target: "mesh::index_cache",
            "Index cache over quota ({} > {} bytes): evicted {} least recently used entries, now {} bytes.",
            size_before,
            self.max_size_bytes,
            evicted,
            size_after
        );
        Ok(QuotaOutcome {
            size_before,
            size_after,
            evicted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn key(i: u32) -> [u8; 32] {
        sha256(&i.to_le_bytes())
    }

    fn rows(cache: &PersistentIndexCache, table: &str) -> i64 {
        let inner = cache.inner.lock().expect("lock");
        inner
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect("count")
    }

    #[test]
    fn miss_then_hit_after_put() {
        let cache = PersistentIndexCache::in_memory().expect("open");
        let hash = [7u8; 32];
        let fp = [9u8; 32];
        let key = PersistentIndexCache::key_for(Path::new("/a/b.rs"), &hash, 0, &fp);
        assert!(cache.get(&key).is_none());
        cache.put_batch(&[(key, b"payload".to_vec())]);
        assert_eq!(cache.get(&key), Some(b"payload".to_vec()));
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.errors), (1, 1, 0));
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

    #[test]
    fn workspace_db_path_is_scoped_by_workspace_id() {
        let a = PersistentIndexCache::workspace_db_path("aaaa");
        let b = PersistentIndexCache::workspace_db_path("bbbb");
        assert_ne!(a, b);
        assert!(a.ends_with("workspaces/aaaa/index-cache.db"));
        assert_ne!(a, PersistentIndexCache::legacy_global_db_path());
    }

    #[test]
    fn new_database_uses_incremental_auto_vacuum_and_current_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = PersistentIndexCache::open(dir.path().join("c.db"), 1 << 30).expect("open");
        let inner = cache.inner.lock().expect("lock");
        let av: i64 = inner
            .conn
            .query_row("PRAGMA auto_vacuum;", [], |r| r.get(0))
            .expect("pragma");
        let uv: i64 = inner
            .conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(av, 2, "auto_vacuum must be INCREMENTAL");
        assert_eq!(uv, i64::from(SCHEMA_VERSION));
    }

    #[test]
    fn database_from_an_older_layout_is_recreated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("c.db");
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE file_index_cache (cache_key BLOB PRIMARY KEY, payload BLOB NOT NULL, updated_at INTEGER NOT NULL);
                 INSERT INTO file_index_cache VALUES (x'00', x'01', 0);",
            )
            .expect("v2 layout");
        }
        let cache = PersistentIndexCache::open(path, 1 << 30).expect("reopen");
        assert_eq!(rows(&cache, "file_index_cache"), 0);
        let inner = cache.inner.lock().expect("lock");
        let av: i64 = inner
            .conn
            .query_row("PRAGMA auto_vacuum;", [], |r| r.get(0))
            .expect("pragma");
        assert_eq!(av, 2);
    }

    #[test]
    fn access_times_are_flushed_in_batch_not_per_read() {
        let cache = PersistentIndexCache::in_memory().expect("open");
        cache.put_batch(&[(key(1), vec![1]), (key(2), vec![2])]);
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.get(&key(1)).is_some());
        {
            let inner = cache.inner.lock().expect("lock");
            assert_eq!(inner.touched.len(), 2, "reads are only recorded in memory");
        }
        cache.put_batch(&[]);
        let inner = cache.inner.lock().expect("lock");
        assert!(
            inner.touched.is_empty(),
            "an empty batch still flushes access times"
        );
    }

    /// Plan 4 step 4.4 exit criterion: inserting past a 10 MB test quota brings the database
    /// (file + WAL) back to at most 80 % of the quota, evicting least recently used entries
    /// first.
    #[test]
    fn inserts_past_a_10mb_quota_shrink_back_under_80_percent() {
        const QUOTA: u64 = 10 * 1024 * 1024;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index-cache.db");
        let cache = PersistentIndexCache::open(path.clone(), QUOTA).expect("open");

        // ~1 KB of incompressible-looking payload per entry, as measured for real
        // `FileIndex` rows (docs/quality.md, step 4.4).
        let payload = |i: u32| -> Vec<u8> {
            (0..32u32)
                .flat_map(|j| sha256(&(i * 64 + j).to_le_bytes()))
                .collect()
        };
        let mut next = 0u32;
        // The very first batch is marked hot: it is read before every later batch, so LRU
        // eviction must keep it.
        let hot: Vec<u32> = (0..50).collect();
        cache.put_batch(
            &hot.iter()
                .map(|&i| (key(i), payload(i)))
                .collect::<Vec<_>>(),
        );
        next += 50;
        let mut max_seen = 0u64;
        for _ in 0..50 {
            for &i in &hot {
                assert!(cache.get(&key(i)).is_some(), "hot entry {i} was evicted");
            }
            let batch: Vec<CacheEntry> = (next..next + 500).map(|i| (key(i), payload(i))).collect();
            next += 500;
            // The hot keys' access bump is flushed in the same transaction (same
            // millisecond timestamp) as the newest writes, so they are never the oldest.
            cache.put_batch(&batch);
            max_seen = max_seen.max(cache.size_bytes());
            assert!(
                cache.size_bytes() <= QUOTA,
                "size {} exceeds quota {QUOTA} after a checked batch",
                cache.size_bytes()
            );
        }
        let written = u64::from(next) * 1024;
        assert!(written > 2 * QUOTA, "test must write well past the quota");
        let stats = cache.stats();
        assert!(stats.evicted > 0, "eviction must have happened");
        assert_eq!(stats.errors, 0);
        // Force a fresh over-quota check to observe the 80 % target directly.
        let batch: Vec<CacheEntry> = (next..next + 3000).map(|i| (key(i), payload(i))).collect();
        cache.put_batch(&batch);
        let size = cache.size_bytes();
        assert!(
            size <= QUOTA / 100 * 80,
            "size {size} after eviction must be <= 80% of {QUOTA} (max seen {max_seen})"
        );
        assert_eq!(size, file_len(&path) + file_len(&wal_path(&path)));
        for &i in &hot {
            assert!(cache.get(&key(i)).is_some(), "hot entry {i} was evicted");
        }
        assert_eq!(
            rows(&cache, "file_index_cache"),
            rows(&cache, "file_index_access"),
            "every payload row keeps exactly one access row"
        );
    }

    #[test]
    fn reopening_over_quota_evicts_on_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index-cache.db");
        {
            let cache = PersistentIndexCache::open(path.clone(), 1 << 30).expect("open");
            let batch: Vec<CacheEntry> = (0..4000u32).map(|i| (key(i), vec![7u8; 1024])).collect();
            cache.put_batch(&batch);
        }
        let quota = 2 * 1024 * 1024;
        let cache = PersistentIndexCache::open(path, quota).expect("reopen");
        assert!(cache.stats().evicted > 0);
        assert!(cache.size_bytes() <= quota / 100 * 80);
    }
}
