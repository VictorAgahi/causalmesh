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
        let canonical_check = PathBuf::from(canonical.to_string_lossy().to_lowercase());
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let canonical_check = &canonical;

        let is_jailed = allowed_roots.iter().any(|root| {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                let root_check = PathBuf::from(root.to_string_lossy().to_lowercase());
                canonical_check.starts_with(&root_check)
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                canonical_check.starts_with(root)
            }
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
    fn test_sibling_prefix_attack_rejected() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let allowed_service = temp_dir.path().join("service");
        let sibling_secret = temp_dir.path().join("service_secret");

        fs::create_dir_all(&allowed_service).expect("create allowed_service");
        fs::create_dir_all(&sibling_secret).expect("create sibling_secret");

        let canonical_allowed = dunce::canonicalize(&allowed_service).expect("canonical allowed");
        let canonical_sibling = dunce::canonicalize(&sibling_secret).expect("canonical sibling");

        let result =
            ValidatedScope::resolve(&canonical_sibling.to_string_lossy(), &[canonical_allowed]);

        assert!(
            matches!(result, Err(SecurityError::SandboxEscapeAttempt(_))),
            "Expected SandboxEscapeAttempt for sibling prefix, got: {:?}",
            result
        );
    }

    #[test]
    fn test_symlink_escape_attempt() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let allowed_dir = temp_dir.path().join("allowed");
        let secret_dir = temp_dir.path().join("secret");

        fs::create_dir_all(&allowed_dir).expect("create allowed");
        fs::create_dir_all(&secret_dir).expect("create secret");

        let secret_file = secret_dir.join("passwords.txt");
        fs::write(&secret_file, "super_secret_token").expect("write secret file");

        let canonical_allowed = dunce::canonicalize(&allowed_dir).expect("canonical allowed");
        let scope = ValidatedScope(canonical_allowed.clone());

        #[cfg(unix)]
        {
            let symlink_path = allowed_dir.join("symlink_to_secret.txt");
            let _ = std::os::unix::fs::symlink(&secret_file, &symlink_path);

            let res = scope.validate_file_access(&symlink_path, &[canonical_allowed]);
            assert!(
                matches!(res, Err(SecurityError::SandboxEscapeAttempt(_))),
                "Symlink pointing outside allowed root must be rejected, got: {:?}",
                res
            );
        }
    }
}
