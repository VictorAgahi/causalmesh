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

        let mtime_nanos = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let file_size = metadata.len();

        if let Some(existing) = self.signatures.get(path) {
            if existing.mtime_nanos == mtime_nanos && existing.file_size == file_size {
                return false; // Fast path: mtime and file size match exactly
            }
        }

        // Slow path: compute cryptographic hash to avoid false positives (e.g. touch or formatting without diff)
        let hash = ring::digest::digest(&ring::digest::SHA256, content.as_bytes());
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(hash.as_ref());

        if let Some(existing) = self.signatures.get(path) {
            if existing.content_hash == content_hash {
                // Content is identical even if mtime was touched; update mtime to avoid rehashing
                self.signatures.insert(
                    path.to_path_buf(),
                    FileSignature {
                        mtime_nanos,
                        file_size,
                        content_hash,
                    },
                );
                return false;
            }
        }

        self.signatures.insert(
            path.to_path_buf(),
            FileSignature {
                mtime_nanos,
                file_size,
                content_hash,
            },
        );
        true
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

        // Simulate modification: write different content to disk and change mtime
        // We must re-create to force a new mtime
        std::fs::write(&path, "content v2").unwrap();

        // Third check with modified content: must detect as changed
        let is_changed_third = vfs.check_and_update(&path, "content v2");
        assert!(is_changed_third, "Modified content must return true");

        // Fourth check after modification with same content: unchanged
        let is_changed_fourth = vfs.check_and_update(&path, "content v2");
        assert!(
            !is_changed_fourth,
            "Same content after update must return false"
        );

        // Remove
        assert!(vfs.remove(&path));
        assert_eq!(vfs.len(), 0);
    }
}
