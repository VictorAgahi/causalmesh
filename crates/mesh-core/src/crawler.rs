use crate::security::ValidatedScope;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub struct FilesystemCrawler;

impl FilesystemCrawler {
    /// Crawls a validated scope enforcing `follow_links(false)` and maximum depth limit.
    pub fn crawl_scope(
        scope: &ValidatedScope,
        exclude_patterns: &[String],
        max_depth: Option<usize>,
    ) -> Vec<PathBuf> {
        let root = scope.as_path();
        let mut builder = WalkBuilder::new(root);

        // Invariant: NEVER follow symlinks (prevents sandbox breakout attacks)
        builder.follow_links(false);
        builder.max_depth(Some(max_depth.unwrap_or(10)));
        builder.git_ignore(true);
        builder.hidden(false); // Scan hidden folders like .github, .agents, but exclude .git

        let mut files = Vec::new();
        let mut visited_symlink_targets = std::collections::HashSet::new();
        let walker = builder.build();

        for result in walker {
            match result {
                Ok(entry) => {
                    let path = entry.path();
                    let is_symlink = entry.path_is_symlink();
                    if !is_symlink {
                        if entry.file_type().is_some_and(|ft| ft.is_file())
                            && !Self::is_excluded(path, exclude_patterns)
                        {
                            files.push(path.to_path_buf());
                        }
                    } else {
                        // Monorepo support (pnpm / Turborepo / Nx):
                        // If symlink target is strictly inside the validated scope root, follow it safely.
                        // External symlinks escaping the scope are strictly discarded.
                        if let Ok(canonical) = dunce::canonicalize(path) {
                            let root_nfc = crate::security::to_nfc_path(root);
                            let canonical_nfc = crate::security::to_nfc_path(&canonical);
                            if canonical_nfc.starts_with(&root_nfc) {
                                if canonical.is_file() {
                                    if !Self::is_excluded(&canonical, exclude_patterns) {
                                        files.push(canonical);
                                    }
                                } else if canonical.is_dir()
                                    && visited_symlink_targets.insert(canonical.clone())
                                {
                                    let sub_scope = match ValidatedScope::resolve(
                                        &canonical.to_string_lossy(),
                                        std::slice::from_ref(&root.to_path_buf()),
                                    ) {
                                        Ok(s) => s,
                                        Err(_) => continue,
                                    };
                                    let sub_files =
                                        Self::crawl_scope(&sub_scope, exclude_patterns, Some(5));
                                    files.extend(sub_files);
                                }
                            } else {
                                tracing::debug!(
                                    target: "mesh::crawler",
                                    "Skipping external symlink target outside workspace: {}",
                                    canonical.display()
                                );
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(target: "mesh::crawler", "Skipping inaccessible entry: {err}");
                }
            }
        }

        files.sort();
        files.dedup();
        files
    }

    fn is_excluded(path: &Path, exclude_patterns: &[String]) -> bool {
        let path_str = path.to_string_lossy();

        // Built-in hard exclusions
        if path_str.contains("/.git/") || path_str.ends_with("/.git") {
            return true;
        }

        for pattern in exclude_patterns {
            let clean_pat = pattern.trim_start_matches("**/").trim_end_matches("/**");
            if clean_pat.contains('*') {
                let prefix = clean_pat.trim_end_matches('*');
                if path_str.contains(prefix) {
                    return true;
                }
            } else if path_str.contains(clean_pat) {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn test_crawler_excludes() {
        let temp_dir = std::env::temp_dir().join("crawler_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_file = temp_dir.join("test.txt");
        let secret_file = temp_dir.join(".env");
        let _ = File::create(&test_file);
        let _ = File::create(&secret_file);

        let allowed = dunce::canonicalize(&temp_dir).unwrap();
        let scope =
            ValidatedScope::resolve(&temp_dir.to_string_lossy(), std::slice::from_ref(&allowed))
                .unwrap();

        let files = FilesystemCrawler::crawl_scope(&scope, &[".env".to_string()], Some(2));
        assert!(files.iter().any(|p| p.ends_with("test.txt")));
        assert!(!files.iter().any(|p| p.ends_with(".env")));
    }

    #[cfg(unix)]
    #[test]
    fn test_crawler_intra_workspace_symlinks() {
        let temp_dir = std::env::temp_dir().join("crawler_symlinks_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);

        let pkg_dir = temp_dir.join("packages").join("common-models");
        let _ = std::fs::create_dir_all(&pkg_dir);
        let model_file = pkg_dir.join("user.ts");
        std::fs::write(&model_file, "export interface User { id: string; }").unwrap();

        // Simulate pnpm monorepo symlink: node_modules/@company/common-models -> ../../packages/common-models
        let nm_dir = temp_dir.join("node_modules").join("@company");
        let _ = std::fs::create_dir_all(&nm_dir);
        let internal_symlink = nm_dir.join("common-models");
        std::os::unix::fs::symlink(&pkg_dir, &internal_symlink).unwrap();

        // Simulate external rogue symlink escaping to /etc
        let external_symlink = temp_dir.join("external_escape");
        let _ = std::os::unix::fs::symlink(std::path::Path::new("/etc"), &external_symlink);

        let allowed = dunce::canonicalize(&temp_dir).unwrap();
        let scope =
            ValidatedScope::resolve(&temp_dir.to_string_lossy(), std::slice::from_ref(&allowed))
                .unwrap();

        let files = FilesystemCrawler::crawl_scope(&scope, &[], Some(5));

        // The internal symlinked target file must be crawled
        assert!(
            files.iter().any(|p| p.ends_with("user.ts")),
            "Internal monorepo symlink target must be included in crawl results"
        );

        // The external escape must NEVER be traversed
        assert!(
            !files.iter().any(|p| p.to_string_lossy().contains("/etc/")),
            "External symlink escaping workspace must be discarded"
        );
    }
}
