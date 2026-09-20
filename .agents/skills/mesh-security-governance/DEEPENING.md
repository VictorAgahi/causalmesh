# DEEPENING: Defensive Sandboxing, Path Case-Folding & Cryptographic Auditing

This document provides deep technical reference material for the `mesh-security-governance` skill.

---

## 1. APFS & NTFS Case-Folding Exploits

On case-insensitive filesystems (macOS APFS default, Windows NTFS):
- The filesystem treats `/Users/dev/PROJECT` and `/Users/dev/project` as the exact same physical inode.
- However, standard Rust `PathBuf::starts_with` or string prefix checks perform byte-by-byte comparisons.

### The Vulnerability:
If `allowed_roots` contains `/Users/dev/project`, an agent passing `/Users/dev/PROJECT/../../etc/passwd` could bypass naive prefix checks if canonicalization or comparison fails to account for case folding.

### The Defense:
In [`ValidatedScope`](../../../crates/mesh-core/src/security.rs):
```rust
#[cfg(any(target_os = "windows", target_os = "macos"))]
let canonical_check = PathBuf::from(canonical.to_string_lossy().to_lowercase());

let is_valid = allowed_roots.iter().any(|root| {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let root_check = PathBuf::from(root.to_string_lossy().to_lowercase());
        canonical_check.starts_with(root_check)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        canonical.starts_with(root)
    }
});
```
This guarantees that prefix evaluation is immune to case-manipulation bypasses on macOS and Windows.

---

## 2. Symlink Traversal & TOCTOU Defense

A common vector for agent sandbox escape is symlinks inside dependency directories (e.g. `node_modules/malicious_pkg/link -> /etc/`).

### MeshMCP Protections:
1. **Dunce Canonicalization**: `dunce::canonicalize()` resolves symlinks to their ultimate target destination before the prefix check executes. If a symlink points outside `allowed_roots`, `starts_with()` immediately fails.
2. **Crawler Invariant**: Filesystem walks (`ignore::WalkBuilder`) explicitly set:
   ```rust
   builder.follow_links(false);
   ```
   Symlinks are never traversed during directory crawling or indexing.

---

## 3. Cryptographic SHA-256 Hash Chaining Verification

The append-only audit trail in `~/.cache/mesh-mcp/audit.log` guarantees non-repudiation:

```text
Hash_n = SHA256(Hash_{n-1} || Timestamp || SessionId || Tool || PayloadDigest)
```

### Log Format:
```
<prev_hash> <timestamp> <session_id> <tool_name> <payload_digest> <current_hash>
```

### Mathematical Replay Verification:
To verify that an audit log has not been tampered with:
1. Initialize `expected_prev_hash = "0000000000000000000000000000000000000000000000000000000000000000"`.
2. For each line in `audit.log`:
   - Assert `line.prev_hash == expected_prev_hash`.
   - Compute `recalculated_hash = sha256(format!("{}|{}|{}|{}|{}", prev, ts, session, tool, digest))`.
   - Assert `recalculated_hash == line.current_hash`.
   - Update `expected_prev_hash = line.current_hash`.
3. Any inserted, deleted, or altered record breaks the chain for all subsequent lines.
