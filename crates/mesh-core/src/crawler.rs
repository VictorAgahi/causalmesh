use crate::security::ValidatedScope;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub struct FilesystemCrawler;

/// Compiled exclusion matcher built once per crawl from the workspace `exclude_patterns`.
///
/// Patterns are gitignore-style globs (`**/node_modules/**`, `**/*.pem`, `.env*`). A pattern
/// without a slash is matched against the basename of every path component, so `build`
/// excludes a `build/` directory anywhere but not `build.rs` or `build_tools/`. Directories
/// matched by a pattern are pruned from the walk entirely instead of being filtered file by file.
pub struct ExcludeMatcher {
    set: GlobSet,
}

impl ExcludeMatcher {
    pub fn compile(exclude_patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for raw in exclude_patterns {
            let pat = raw.trim();
            if pat.is_empty() {
                continue;
            }
            // Normalize to a path-anchored form so `foo/**` and `**/foo/**` both prune `foo/`
            // and everything under it, and `*.pem` matches at any depth.
            let mut expanded: Vec<String> = Vec::with_capacity(3);
            let core = pat.trim_end_matches("/**").trim_end_matches('/');
            let anchored = if core.starts_with("**/") || core.starts_with('/') {
                core.to_string()
            } else {
                format!("**/{core}")
            };
            expanded.push(anchored.clone());
            expanded.push(format!("{anchored}/**"));

            for e in expanded {
                match Glob::new(&e) {
                    Ok(g) => {
                        builder.add(g);
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "mesh::crawler",
                            "Ignoring invalid exclude pattern {raw:?}: {err}"
                        );
                    }
                }
            }
        }
        let set = builder.build().unwrap_or_else(|err| {
            tracing::error!(target: "mesh::crawler", "Exclude set failed to compile: {err}");
            GlobSet::empty()
        });
        Self { set }
    }

    /// `rel` must be the path relative to the crawl root (never absolute) so that user
    /// patterns can't accidentally match the workspace's own parent directories.
    #[inline]
    pub fn is_excluded(&self, rel: &Path) -> bool {
        // `.git` is always off-limits regardless of configuration.
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return true;
        }
        self.set.is_match(rel)
    }
}

impl FilesystemCrawler {
    /// Crawls a validated scope enforcing `follow_links(false)` and maximum depth limit.
    pub fn crawl_scope(
        scope: &ValidatedScope,
        exclude_patterns: &[String],
        max_depth: Option<usize>,
    ) -> Vec<PathBuf> {
        let matcher = ExcludeMatcher::compile(exclude_patterns);
        Self::crawl_scope_with(scope, &matcher, max_depth)
    }

    fn crawl_scope_with(
        scope: &ValidatedScope,
        matcher: &ExcludeMatcher,
        max_depth: Option<usize>,
    ) -> Vec<PathBuf> {
        let root = scope.as_path();
        let mut builder = WalkBuilder::new(root);

        // Invariant: NEVER follow symlinks (prevents sandbox breakout attacks)
        builder.follow_links(false);
        builder.max_depth(Some(max_depth.unwrap_or(10)));
        builder.git_ignore(true);
        builder.hidden(false); // Scan hidden folders like .github, .agents, but exclude .git

        // Prune excluded directories at the walker level so `node_modules/` is never descended.
        let root_for_filter = root.to_path_buf();
        let filter_set = matcher.set.clone();
        builder.filter_entry(move |entry| {
            let rel = match entry.path().strip_prefix(&root_for_filter) {
                Ok(r) => r,
                Err(_) => return true,
            };
            if rel.as_os_str().is_empty() {
                return true;
            }
            if rel.components().any(|c| c.as_os_str() == ".git") {
                return false;
            }
            !filter_set.is_match(rel)
        });

        let mut files = Vec::new();
        let mut visited_symlink_targets = std::collections::HashSet::new();
        let root_nfc = crate::security::to_nfc_path(root);
        let walker = builder.build();

        for result in walker {
            match result {
                Ok(entry) => {
                    let path = entry.path();
                    let is_symlink = entry.path_is_symlink();
                    if !is_symlink {
                        // Exclusion already enforced by filter_entry for non-symlink entries.
                        if entry.file_type().is_some_and(|ft| ft.is_file()) {
                            files.push(path.to_path_buf());
                        }
                    } else {
                        // Monorepo support (pnpm / Turborepo / Nx):
                        // If symlink target is strictly inside the validated scope root, follow it safely.
                        // External symlinks escaping the scope are strictly discarded.
                        if let Ok(canonical) = dunce::canonicalize(path) {
                            let canonical_nfc = crate::security::to_nfc_path(&canonical);
                            if canonical_nfc.starts_with(&root_nfc) {
                                let rel_target = canonical.strip_prefix(root).unwrap_or(&canonical);
                                if matcher.is_excluded(rel_target) {
                                    continue;
                                }
                                if canonical.is_file() {
                                    files.push(canonical);
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
                                    // The sub-crawl is rooted at the target, so its relative
                                    // paths are computed from there; reuse the same pattern set.
                                    let sub_files =
                                        Self::crawl_scope_with(&sub_scope, matcher, Some(5));
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

    #[test]
    fn test_default_exclude_patterns_are_real_globs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ws = temp.path();
        let touch = |rel: &str| {
            let p = ws.join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(&p, "x").expect("write");
        };
        // Must be excluded by the defaults
        touch("certs/server.pem");
        touch("certs/server.key");
        touch("certs/store.jks");
        touch("secrets/db.yaml");
        touch("node_modules/pkg/index.js");
        touch("target/debug/app.rs");
        touch("build/out.rs");
        touch(".env");
        touch("id_rsa");
        // Must be kept: substring collisions with `build`, `node_modules`
        touch("src/build.rs");
        touch("build_tools/keep.rs");
        touch("my_node_modules_backup/keep.rs");
        touch("src/main.rs");

        let cfg = crate::config::Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"2.9.0\"\nroots = [\".\"]\n",
        )
        .expect("config");
        let allowed = dunce::canonicalize(ws).expect("canon");
        let scope =
            ValidatedScope::resolve(&allowed.to_string_lossy(), std::slice::from_ref(&allowed))
                .expect("scope");

        let files =
            FilesystemCrawler::crawl_scope(&scope, &cfg.workspace.exclude_patterns, Some(10));
        let rel: Vec<String> = files
            .iter()
            .map(|f| {
                f.strip_prefix(&allowed)
                    .expect("rel")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        for kept in [
            "src/build.rs",
            "build_tools/keep.rs",
            "my_node_modules_backup/keep.rs",
            "src/main.rs",
        ] {
            assert!(
                rel.iter().any(|r| r == kept),
                "{kept} must be crawled, got {rel:?}"
            );
        }
        for excluded in [
            "certs/server.pem",
            "certs/server.key",
            "certs/store.jks",
            "secrets/db.yaml",
            "node_modules/pkg/index.js",
            "target/debug/app.rs",
            "build/out.rs",
            ".env",
            "id_rsa",
        ] {
            assert!(
                !rel.iter().any(|r| r == excluded),
                "{excluded} must be excluded, got {rel:?}"
            );
        }
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
