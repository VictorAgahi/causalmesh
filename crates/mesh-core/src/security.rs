use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

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

/// Helper normalizing any Path to Unicode Normalization Form C (NFC)
#[inline]
pub fn to_nfc_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let nfc: String = s.nfc().collect();
    PathBuf::from(nfc)
}

/// A validated, canonicalized, and sandboxed path jail per RFC-001 Commandment 4.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValidatedScope(PathBuf);

impl ValidatedScope {
    /// Resolves and canonicalizes a raw scope path, asserting it is jailed within one of `allowed_roots`.
    pub fn resolve(raw_scope: &str, allowed_roots: &[PathBuf]) -> Result<Self, SecurityError> {
        Self::resolve_with_aliases(raw_scope, allowed_roots, &HashMap::new(), None)
    }

    /// Resolves and canonicalizes a raw scope path with Docker bind-mount alias translation and Unicode NFC normalization.
    ///
    /// A relative scope (after alias translation) is joined onto `anchor` — the
    /// resolved workspace root — *before* canonicalization. Without it,
    /// `dunce::canonicalize` would evaluate the scope against the process CWD,
    /// which for an IDE-spawned server is arbitrary (often `~`), so
    /// `scope: "crates/mesh-core"` failed with `PathNotFound`. Anchoring does not
    /// widen the jail: the canonical result must still sit under an allowed root.
    pub fn resolve_with_aliases(
        raw_scope: &str,
        allowed_roots: &[PathBuf],
        mount_aliases: &HashMap<String, String>,
        anchor: Option<&Path>,
    ) -> Result<Self, SecurityError> {
        // Step 1: Normalize input string to Unicode NFC form
        let nfc_input: String = raw_scope.nfc().collect();

        // Step 2: Resolve Docker bind-mount aliases if applicable
        let mut translated_scope = nfc_input.clone();
        for (alias, target) in mount_aliases {
            let alias_nfc: String = alias.nfc().collect();
            let target_nfc: String = target.nfc().collect();
            if nfc_input == alias_nfc {
                translated_scope = target_nfc;
                break;
            } else if nfc_input.starts_with(&format!("{alias_nfc}/")) {
                translated_scope = format!("{}{}", target_nfc, &nfc_input[alias_nfc.len()..]);
                break;
            }
        }

        let clean = match anchor {
            Some(root) if Path::new(&translated_scope).is_relative() => {
                path_clean::clean(root.join(&translated_scope))
            }
            _ => path_clean::clean(&translated_scope),
        };
        let canonical =
            dunce::canonicalize(&clean).map_err(|_| SecurityError::PathNotFound(clean.clone()))?;

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

    /// IDE-spawned servers run with an arbitrary CWD; a relative scope must
    /// resolve against the workspace root anchor, and anchoring must not let a
    /// `..` walk out of the jail.
    #[test]
    fn relative_scope_anchors_on_workspace_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::create_dir_all(root.join("crates/core")).expect("mkdir");
        let roots = [root.clone()];
        let aliases = HashMap::new();

        let scope =
            ValidatedScope::resolve_with_aliases("crates/core", &roots, &aliases, Some(&root))
                .expect("anchored relative scope resolves");
        assert_eq!(scope.as_path(), root.join("crates/core"));

        // Without an anchor the same string is evaluated against the CWD (the
        // crate dir under `cargo test`), where it does not exist.
        assert!(matches!(
            ValidatedScope::resolve_with_aliases("crates/core", &roots, &aliases, None),
            Err(SecurityError::PathNotFound(_))
        ));

        // `.` is the workspace root itself.
        assert!(ValidatedScope::resolve_with_aliases(".", &roots, &aliases, Some(&root)).is_ok());

        // Anchoring never widens the jail: `..` from an anchor that is itself the
        // only allowed root lands outside it.
        let jailed = [root.join("crates")];
        assert!(matches!(
            ValidatedScope::resolve_with_aliases(
                "..",
                &jailed,
                &aliases,
                Some(&root.join("crates"))
            ),
            Err(SecurityError::SandboxEscapeAttempt(_))
        ));
    }

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

    #[cfg(unix)]
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

        let symlink_path = allowed_dir.join("symlink_to_secret.txt");
        let _ = std::os::unix::fs::symlink(&secret_file, &symlink_path);

        let res = scope.validate_file_access(&symlink_path, &[canonical_allowed]);
        assert!(
            matches!(res, Err(SecurityError::SandboxEscapeAttempt(_))),
            "Symlink pointing outside allowed root must be rejected, got: {:?}",
            res
        );
    }

    #[test]
    fn test_unicode_nfc_nfd_normalization() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        // Folder name with French accent: "crédit-service"
        // In NFC: \u{00e9} (single code point)
        // In NFD: e + \u{0301} (two code points)
        let nfc_name = "cr\u{00e9}dit-service";
        let nfd_name = "cre\u{0301}dit-service";

        assert_ne!(nfc_name.as_bytes(), nfd_name.as_bytes());

        let service_dir = temp_dir.path().join(nfc_name);
        fs::create_dir_all(&service_dir).expect("create accented dir");

        let canonical_root = dunce::canonicalize(temp_dir.path()).expect("canonical root");

        // Resolve using NFD decomposed string
        let nfd_input = format!("{}/{}", temp_dir.path().display(), nfd_name);
        let scope_res = ValidatedScope::resolve(&nfd_input, std::slice::from_ref(&canonical_root));
        assert!(
            scope_res.is_ok(),
            "NFD input must resolve cleanly against NFC canonical path: {:?}",
            scope_res
        );

        // Resolve using NFC composed string
        let nfc_input = format!("{}/{}", temp_dir.path().display(), nfc_name);
        let scope_res2 = ValidatedScope::resolve(&nfc_input, std::slice::from_ref(&canonical_root));
        assert!(
            scope_res2.is_ok(),
            "NFC input must resolve cleanly: {:?}",
            scope_res2
        );
    }

    #[test]
    fn test_docker_mount_alias_resolution() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let sub_service = temp_dir.path().join("services").join("billing");
        fs::create_dir_all(&sub_service).expect("create billing service");

        let canonical_root = dunce::canonicalize(temp_dir.path()).expect("canonical root");

        let mut aliases = HashMap::new();
        aliases.insert(
            "/workspace".to_string(),
            temp_dir.path().to_string_lossy().into_owned(),
        );

        let result = ValidatedScope::resolve_with_aliases(
            "/workspace/services/billing",
            &[canonical_root],
            &aliases,
            None,
        );

        assert!(
            result.is_ok(),
            "Docker bind mount alias /workspace must be resolved to temp dir"
        );
        let scope = result.expect("resolved scope");
        assert!(scope.as_path().ends_with("billing"));
    }

    #[test]
    fn test_parent_directory_access_strictly_rejected() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let parent_root = temp_dir.path().join("monorepo");
        let allowed_service = parent_root.join("services").join("checkout");
        let other_service = parent_root.join("services").join("auth");

        fs::create_dir_all(&allowed_service).expect("create checkout");
        fs::create_dir_all(&other_service).expect("create auth");

        let canonical_allowed = dunce::canonicalize(&allowed_service).expect("canonical allowed");
        let canonical_parent = dunce::canonicalize(&parent_root).expect("canonical parent");

        // Attempting to resolve parent monorepo directory when only a subservice is allowed
        // MUST fail with SandboxEscapeAttempt.
        let result =
            ValidatedScope::resolve(&canonical_parent.to_string_lossy(), &[canonical_allowed]);
        assert!(
            matches!(result, Err(SecurityError::SandboxEscapeAttempt(_))),
            "Parent directory must NEVER be allowed when only child is jailed: {:?}",
            result
        );
    }
}
