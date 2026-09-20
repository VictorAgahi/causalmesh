use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SecurityError {
    #[error("Sandbox escape attempt detected: {0}")]
    SandboxEscapeAttempt(PathBuf),

    #[error("Path not found: {0}")]
    PathNotFound(PathBuf),

    #[error("Broken or unresolvable symlink: {0}")]
    BrokenSymlink(PathBuf),

    #[error("Prohibited root directory: {0}")]
    ProhibitedRoot(PathBuf),

    #[error("Path normalization failure for {0}: {1}")]
    NormalizationFailed(String, String),
}

impl SecurityError {
    /// JSON-RPC error code per RFC-001 Commandment 4
    pub fn jsonrpc_code(&self) -> i32 {
        -32602
    }
}

/// A validated, canonicalized, and sandboxed path jail per RFC-001 Commandment 4.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValidatedScope(PathBuf);

impl ValidatedScope {
    /// Resolves and canonicalizes a raw scope path, asserting it is jailed within one of `allowed_roots`.
    pub fn resolve(raw_scope: &str, allowed_roots: &[PathBuf]) -> Result<Self, SecurityError> {
        let clean = path_clean::clean(raw_scope);
        let canonical =
            dunce::canonicalize(&clean).map_err(|_| SecurityError::PathNotFound(clean.clone()))?;

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let canonical_cmp = canonical.to_string_lossy().to_lowercase();
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let canonical_cmp = canonical.to_string_lossy();

        let is_jailed = allowed_roots.iter().any(|root| {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            let root_cmp = root.to_string_lossy().to_lowercase();
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            let root_cmp = root.to_string_lossy();

            canonical_cmp.starts_with(root_cmp.as_str())
        });

        if is_jailed {
            Ok(Self(canonical))
        } else {
            tracing::warn!(
                target: "mesh::security",
                "Sandbox escape attempt: {}",
                canonical.display()
            );
            Err(SecurityError::SandboxEscapeAttempt(canonical))
        }
    }

    /// Validates an individual file access against the sandbox and symlink invariants.
    pub fn validate_file_access(
        &self,
        file_path: &Path,
        allowed_roots: &[PathBuf],
    ) -> Result<PathBuf, SecurityError> {
        let symlink_metadata = fs::symlink_metadata(file_path)
            .map_err(|_| SecurityError::PathNotFound(file_path.to_path_buf()))?;

        let canonical = dunce::canonicalize(file_path).map_err(|_| {
            if symlink_metadata.file_type().is_symlink() {
                SecurityError::BrokenSymlink(file_path.to_path_buf())
            } else {
                SecurityError::PathNotFound(file_path.to_path_buf())
            }
        })?;

        Self::resolve(&canonical.to_string_lossy(), allowed_roots)?;
        Ok(canonical)
    }

    #[inline]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[inline]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for ValidatedScope {
    #[inline]
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::fmt::Display for ValidatedScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scope_jail_success() {
        let temp_dir = std::env::temp_dir();
        let allowed_root = dunce::canonicalize(&temp_dir).expect("valid temp dir");
        let scope = ValidatedScope::resolve(&temp_dir.to_string_lossy(), &[allowed_root]);
        assert!(scope.is_ok());
    }

    #[test]
    fn test_scope_jail_escape_attempt() {
        let temp_dir = std::env::temp_dir();
        let allowed_root = temp_dir.join("subfolder_allowed");
        let parent = temp_dir.clone();

        let result = ValidatedScope::resolve(&parent.to_string_lossy(), &[allowed_root]);
        assert!(matches!(
            result,
            Err(SecurityError::SandboxEscapeAttempt(_))
        ));
    }
}
