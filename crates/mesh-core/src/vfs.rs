use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Cache entry representing the cryptographic and filesystem signature of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSignature {
    pub mtime_nanos: u128,
    pub file_size: u64,
    pub content_hash: [u8; 32],
}

/// A lightweight, lock-free capable virtual filesystem cache tracking file modifications.
#[derive(Debug, Clone, Default)]
pub struct DifferentialVfs {
    signatures: HashMap<PathBuf, FileSignature>,
}

impl DifferentialVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if a file at `path` with `content` has changed compared to its cached signature.
    /// Returns `true` if new or modified, `false` if strictly unchanged.
    /// Updates internal signature on change.
    pub fn check_and_update(&mut self, path: &Path, content: &str) -> bool {
        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return true,
        };
        if self.is_unchanged_fast(path, &metadata) {
            return false;
        }
        let sig = Self::compute_signature(&metadata, content.as_bytes());
        self.upsert(path, sig)
    }

    /// Fast path: `true` when mtime and size both match the cached signature, so the
    /// file need not even be read. Call this *before* `fs::read` on a reload.
    #[inline]
    pub fn is_unchanged_fast(&self, path: &Path, metadata: &std::fs::Metadata) -> bool {
        let (mtime_nanos, file_size) = Self::stat_parts(metadata);
        self.signatures
            .get(path)
            .is_some_and(|s| s.mtime_nanos == mtime_nanos && s.file_size == file_size)
    }

    /// Builds the full signature (stat + SHA-256). Pure — safe on any thread.
    pub fn compute_signature(metadata: &std::fs::Metadata, content: &[u8]) -> FileSignature {
        let (mtime_nanos, file_size) = Self::stat_parts(metadata);
        let hash = ring::digest::digest(&ring::digest::SHA256, content);
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(hash.as_ref());
        FileSignature {
            mtime_nanos,
            file_size,
            content_hash,
        }
    }

    /// Records `sig` for `path`. Returns `true` if the content hash is new or differs
    /// from the cached one (i.e. the file must be re-indexed); a bare `touch` returns `false`.
    pub fn upsert(&mut self, path: &Path, sig: FileSignature) -> bool {
        let changed = self
            .signatures
            .get(path)
            .is_none_or(|existing| existing.content_hash != sig.content_hash);
        self.signatures.insert(path.to_path_buf(), sig);
        changed
    }

    /// Paths currently tracked, for detecting deletions against a fresh crawl.
    pub fn tracked_paths(&self) -> impl Iterator<Item = &Path> {
        self.signatures.keys().map(PathBuf::as_path)
    }

    #[inline]
    fn stat_parts(metadata: &std::fs::Metadata) -> (u128, u64) {
        let mtime_nanos = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        (mtime_nanos, metadata.len())
    }

    /// Removes a file from the VFS cache (e.g. when deleted from disk).
    pub fn remove(&mut self, path: &Path) -> bool {
        self.signatures.remove(path).is_some()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.signatures.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }

    pub fn clear(&mut self) {
        self.signatures.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_vfs_differential_detection() {
        let mut vfs = DifferentialVfs::new();
        let mut file = NamedTempFile::new().unwrap();

        // Write initial content to disk
        write!(file, "content v1").unwrap();
        file.flush().unwrap();
        let path = file.path().to_path_buf();

        // Initial check: must detect as changed / new
        let is_changed = vfs.check_and_update(&path, "content v1");
        assert!(is_changed, "First check must detect as new");
        assert_eq!(vfs.len(), 1);

        // Second check with identical content and no disk change: must detect as unchanged
        let is_changed_second = vfs.check_and_update(&path, "content v1");
        assert!(!is_changed_second, "Unmodified content must return false");

        // Simulate modification: ensure mtime tick has passed on filesystems with low timer granularity (e.g. Windows NTFS ~15ms)
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "content v2 - modified payload").unwrap();

        // Third check with modified content: must detect as changed
        let is_changed_third = vfs.check_and_update(&path, "content v2 - modified payload");
        assert!(is_changed_third, "Modified content must return true");

        // Fourth check after modification with same content: unchanged
        let is_changed_fourth = vfs.check_and_update(&path, "content v2 - modified payload");
        assert!(
            !is_changed_fourth,
            "Same content after update must return false"
        );

        // Fifth check: touch file (mtime changes, but content identical) - should detect as unchanged
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "content v2 - modified payload").unwrap();
        let is_changed_fifth = vfs.check_and_update(&path, "content v2 - modified payload");
        assert!(
            !is_changed_fifth,
            "Touched file with identical hash must return false"
        );

        // Remove
        assert!(vfs.remove(&path));
        assert_eq!(vfs.len(), 0);
    }
}
