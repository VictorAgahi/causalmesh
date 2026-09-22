# DEEPENING: Case-Folding, Symlink Defence & the SQLite Audit Chain

Deep reference for the `mesh-security-governance` skill. Read the skill first.

---

## 1. Case-folding on APFS and NTFS

macOS (APFS, default configuration) and Windows (NTFS) treat `/Users/dev/PROJECT` and
`/Users/dev/project` as the same inode. Rust's `PathBuf::starts_with` compares components
byte by byte, so a byte-exact prefix check on a case-insensitive filesystem can be made to
fail on a path that the OS will happily open.

The defence in
[`ValidatedScope::resolve_with_aliases`](../../../crates/mesh-core/src/security.rs)
normalises both sides, and only on the affected platforms:

```rust
let canonical_nfc = to_nfc_path(&canonical);

#[cfg(any(target_os = "windows", target_os = "macos"))]
let canonical_check = PathBuf::from(canonical_nfc.to_string_lossy().to_lowercase());
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
let canonical_check = &canonical_nfc;

let is_jailed = allowed_roots.iter().any(|root| {
    let root_nfc = to_nfc_path(root);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let root_check = PathBuf::from(root_nfc.to_string_lossy().to_lowercase());
        canonical_check.starts_with(&root_check)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        canonical_check.starts_with(&root_nfc)
    }
});
```

The `cfg` split is load-bearing in both directions. On macOS/Windows, folding closes the
bypass. On Linux, folding would *widen* the jail, because `/srv/App` and `/srv/app` are
two different directories there.

### Unicode normalisation

Both the input string and the canonical result are pushed through NFC
(`unicode_normalization::UnicodeNormalization::nfc`, wrapped by `to_nfc_path`). macOS
historically stores filenames in a decomposed form, so `é` can arrive as `U+00E9` from the
agent and as `U+0065 U+0301` from the filesystem. Without normalisation those are
different byte strings and the prefix check fails on a legitimate path — or, worse,
succeeds on a crafted one. `FilesystemCrawler::crawl_scope` calls `to_nfc_path` on the
crawl root for the same reason.

### Docker bind-mount aliases

`resolve_with_aliases` translates a configured alias prefix before cleaning: an exact
match replaces the whole path, and an `alias/` prefix has its head replaced. This is how a
container path maps onto a host root. Aliases are applied **before** canonicalisation, so
they cannot be used to skip it. `resolve` passes an empty map.

---

## 2. Symlink traversal and TOCTOU

Two independent layers:

1. **Canonicalisation before the check.** `dunce::canonicalize` resolves the full symlink
   chain, so `allowed/link -> /etc` becomes `/etc` before `starts_with` runs and fails the
   jail.
2. **The crawler never follows links.**
   [`FilesystemCrawler::crawl_scope_with`](../../../crates/mesh-core/src/crawler.rs):

   ```rust
   // Invariant: NEVER follow symlinks (prevents sandbox breakout attacks)
   builder.follow_links(false);
   ```

   It also keeps a `visited_symlink_targets` set, sets `git_ignore(true)` and
   `hidden(false)` (so `.github` and `.agents` are scanned while `.git` is excluded
   unconditionally by `ExcludeMatcher::is_excluded`).

`ValidatedScope::validate_file_access` handles the per-file TOCTOU window: it takes
`fs::symlink_metadata` *first*, so that if canonicalisation then fails it can distinguish
a dangling symlink (`BrokenSymlink`) from a missing file (`PathNotFound`), and it re-runs
the full `resolve` on the canonical result rather than trusting the earlier scope check.

An exclusion pattern is matched against the path **relative to the crawl root**, never the
absolute path — otherwise a user pattern could match a component of the workspace's own
parent directory and silently empty the crawl.

---

## 3. Verifying the audit chain

The chain is in SQLite (`audit_entries`), not a text file. `AuditLogger::verify_db` reads
every row ordered by `entry_seq ASC` and enforces three things per row:

1. `entry_seq == idx` — no gaps, no reordering.
2. `prev_hash == expected_prev_hash`, starting from `GENESIS_HASH` (64 zeros).
3. The recomputed hash matches the stored one:

   ```rust
   let hash_input = format!(
       "{}{}{}{}{}",
       entry.prev_hash, entry.timestamp, entry.session_id, entry.tool, entry.args_digest
   );
   let calculated_hash = Self::compute_sha256(hash_input.as_bytes());
   ```

Any failure returns `AuditError::BrokenChain(seq, expected, actual)`. A missing file
verifies as `Ok(true)` — nothing recorded is not the same as something tampered with.

Note what is **not** in the hash input: `status`, `files_accessed` and
`secrets_redacted_count` are stored but not chained. An attacker with write access to the
DB could alter those without breaking the chain. Treat the chained fields
(`timestamp`, `session_id`, `tool`, `args_digest`) as the attested ones. If you add a field
that must be attested, add it to both `record_entry` and `verify_db` in the same commit —
changing one alone invalidates every existing database.

`record_entry` reads the tail inside the same `Immediate` transaction it writes in, which
is what makes concurrent `mesh-mcp` processes and `meshd` sharing one DB safe: SQLite
serialises the writers and each one sees the true committed tail, never a cached
in-memory value.

`export_to_jsonl` writes the rows out for external tooling; it is a dump, not a second
source of truth — verify against the DB.
