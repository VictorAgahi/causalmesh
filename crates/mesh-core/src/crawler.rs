use crate::security::ValidatedScope;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct FilesystemCrawler;

/// Compiled exclusion matcher built once per crawl from the workspace `exclude_patterns`.
///
/// Patterns are gitignore-style globs (`**/node_modules/**`, `**/*.pem`, `.env*`). A pattern
/// without a slash is matched against the basename of every path component, so `build`
/// excludes a `build/` directory anywhere but not `build.rs` or `build_tools/`. Directories
/// matched by a pattern are pruned from the walk entirely instead of being filtered file by file.
#[derive(Clone)]
pub struct ExcludeMatcher {
    set: GlobSet,
    pub raw_patterns: Vec<String>,
    pattern_indices: Vec<usize>,
    hits: Vec<Arc<AtomicUsize>>,
}

impl ExcludeMatcher {
    pub fn compile(exclude_patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut raw_patterns = Vec::new();
        let mut pattern_indices = Vec::new();
        let mut hits = Vec::new();

        for raw in exclude_patterns {
            let pat = raw.trim();
            if pat.is_empty() {
                continue;
            }
            raw_patterns.push(pat.to_string());
            let current_raw_idx = hits.len();
            hits.push(Arc::new(AtomicUsize::new(0)));

            // Normalize and expand `${workspace_root}` / `${WORKSPACE_ROOT}` references
            let clean = crate::config::strip_workspace_root_prefix(pat);

            // Normalize to a path-anchored form so `foo/**` and `**/foo/**` both prune `foo/`
            // and everything under it, and `*.pem` matches at any depth.
            let mut expanded: Vec<String> = Vec::with_capacity(4);
            let core = clean.trim_end_matches("/**").trim_end_matches('/');
            if !core.is_empty() {
                let anchored = if core.starts_with("**/") || core.starts_with('/') {
                    core.to_string()
                } else {
                    format!("**/{core}")
                };
                expanded.push(anchored.clone());
                expanded.push(format!("{anchored}/**"));

                if core.starts_with('/') {
                    let unslash = core.trim_start_matches('/');
                    expanded.push(unslash.to_string());
                    expanded.push(format!("{unslash}/**"));
                }
            }

            for e in expanded {
                match Glob::new(&e) {
                    Ok(g) => {
                        builder.add(g);
                        pattern_indices.push(current_raw_idx);
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
        Self {
            set,
            raw_patterns,
            pattern_indices,
            hits,
        }
    }

    /// `rel` must be the path relative to the crawl root (never absolute) so that user
    /// patterns can't accidentally match the workspace's own parent directories.
    #[inline]
    pub fn is_excluded(&self, rel: &Path) -> bool {
        self.is_excluded_with_root(rel, None)
    }

    /// Checks if `rel` (optionally prepended with `root`'s folder name) matches an exclude pattern.
    pub fn is_excluded_with_root(&self, rel: &Path, root: Option<&Path>) -> bool {
        // `.git` is always off-limits regardless of configuration.
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return true;
        }
        let matches = self.set.matches(rel);
        if !matches.is_empty() {
            for m in matches {
                if let Some(&raw_idx) = self.pattern_indices.get(m) {
                    if let Some(h) = self.hits.get(raw_idx) {
                        h.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            return true;
        }
        if let Some(r) = root {
            if let Some(root_name) = r.file_name() {
                let rel_with_root = Path::new(root_name).join(rel);
                let matches_root = self.set.matches(&rel_with_root);
                if !matches_root.is_empty() {
                    for m in matches_root {
                        if let Some(&raw_idx) = self.pattern_indices.get(m) {
                            if let Some(h) = self.hits.get(raw_idx) {
                                h.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    return true;
                }
            }
        }
        false
    }

    /// Returns the raw patterns that matched zero files during scans.
    pub fn unmatched_patterns(&self) -> Vec<&str> {
        let mut unmatched = Vec::new();
        for (idx, raw) in self.raw_patterns.iter().enumerate() {
            if let Some(h) = self.hits.get(idx) {
                if h.load(Ordering::Relaxed) == 0 {
                    unmatched.push(raw.as_str());
                }
            }
        }
        unmatched
    }
}

impl FilesystemCrawler {
    /// Whether one specific, already-known path under `root` would be excluded from
    /// a `crawl_scope_with(root, matcher, ..)` walk — without re-walking the tree to
    /// find out. Combines `matcher` (the compiled `exclude_patterns`, including
    /// nested-root exclusion) with the same `.gitignore` semantics a full crawl
    /// applies (`git_ignore(true)`), so a file-watcher-driven targeted reload can
    /// reuse a full crawl's exact exclusion decision for one path in O(path depth)
    /// stat calls instead of an O(repo size) walk.
    ///
    /// Only `root`'s own top-level `.gitignore`/`.ignore`/`.git/info/exclude` are
    /// honored. If any directory strictly between `root` and `path` carries its
    /// *own* nested `.gitignore` or `.ignore`, this can't cheaply and correctly
    /// reproduce `ignore::WalkBuilder`'s per-directory gitignore stacking (each
    /// nested file's patterns are scoped to its own subtree, composing with every
    /// ancestor's) — rather than risk a false "not excluded" for a file a full
    /// crawl would have pruned, this returns `None` so the caller falls back to a
    /// full crawl. Nested ignore files are real but comparatively rare next to a
    /// single root-level one; this is a deliberate, documented scope limit, not a
    /// silent gap. A user's *global* `core.excludesFile` (outside this repo
    /// entirely) is not consulted at all — detecting its configured path would
    /// mean reading git config, which this deliberately does not attempt; a path
    /// excluded only by a global excludesfile is a known, narrower gap than the
    /// nested-ignore-file fallback above (no signal this function can cheaply
    /// check for forces a fallback for it).
    pub fn is_path_excluded(root: &Path, path: &Path, matcher: &ExcludeMatcher) -> Option<bool> {
        let rel = path.strip_prefix(root).ok()?;
        if matcher.is_excluded_with_root(rel, Some(root)) {
            return Some(true);
        }
        // `path == root` (e.g. a coalesced watcher event landing on the root
        // directory itself) has no ancestor strictly between it and `root` to
        // walk — and no parent to start that walk from without leaving `root`.
        if rel.as_os_str().is_empty() {
            return Some(false);
        }

        let mut dir = path.parent()?.to_path_buf();
        while dir != root {
            if !dir.starts_with(root) {
                // Walked off `root` without matching it by value — `path` wasn't
                // really a descendant despite `strip_prefix` succeeding (e.g. a
                // `..`-relative or otherwise unnormalized input path). Bail out
                // rather than stat directories outside the workspace jail.
                return None;
            }
            if dir.join(".gitignore").is_file() || dir.join(".ignore").is_file() {
                return None;
            }
            if !dir.pop() {
                return None;
            }
        }

        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        let mut had_source = false;
        for name in [".gitignore", ".ignore"] {
            let p = root.join(name);
            if p.is_file() {
                if builder.add(&p).is_some() {
                    return None; // unparseable ignore file — don't guess
                }
                had_source = true;
            }
        }
        let git_exclude = root.join(".git").join("info").join("exclude");
        if git_exclude.is_file() {
            if builder.add(&git_exclude).is_some() {
                return None;
            }
            had_source = true;
        }
        if !had_source {
            return Some(false);
        }
        let gitignore = match builder.build() {
            Ok(gi) => gi,
            Err(_) => return None,
        };
        Some(gitignore.matched(path, path.is_dir()).is_ignore())
    }

    /// Crawls a validated scope enforcing `follow_links(false)` and maximum depth limit.
    pub fn crawl_scope(
        scope: &ValidatedScope,
        exclude_patterns: &[String],
        max_depth: Option<usize>,
    ) -> Vec<PathBuf> {
        let matcher = ExcludeMatcher::compile(exclude_patterns);
        Self::crawl_scope_with(scope, &matcher, max_depth)
    }

    pub fn crawl_scope_with(
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
                               // `ignore::WalkBuilder` otherwise yields directory entries in whatever order the
                               // OS/filesystem returns them (readdir order, not guaranteed stable — ext4, APFS
                               // and NTFS all differ, and even one filesystem can reorder entries after a rename).
                               // Sorting by filename makes the crawl itself reproducible; `canonical_lines()`
                               // downstream still doesn't depend on it, but every other consumer of this file
                               // list (VFS diffing, doctor's dead-config reporting, the sequential parse retry
                               // in `WorkspaceIndexer`) benefits from a stable, reviewable order.
        builder.sort_by_file_name(std::ffi::OsStr::cmp);

        // Prune excluded directories at the walker level so `node_modules/` is never descended.
        let root_for_filter = root.to_path_buf();
        let matcher_clone = matcher.clone();
        builder.filter_entry(move |entry| {
            let rel = match entry.path().strip_prefix(&root_for_filter) {
                Ok(r) => r,
                Err(_) => return true,
            };
            if rel.as_os_str().is_empty() {
                return true;
            }
            !matcher_clone.is_excluded_with_root(rel, Some(&root_for_filter))
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

    /// Every directory under `root` a watcher should individually register (P2 step 3.3):
    /// nested-`.gitignore`-aware (the `ignore` crate discovers and chains ignore files from
    /// `root`'s ancestors up to a repository boundary, the same way `git` itself does, so this
    /// is correct even when called again later on a freshly created subdirectory rather than
    /// the original workspace root — see `FileWatcherService`'s dynamic re-registration) and
    /// `exclude_patterns`-aware, using the exact same walker configuration as
    /// [`Self::crawl_scope_with`] (`follow_links(false)`, `git_ignore(true)`, sorted). A
    /// directory that would be excluded from a crawl is never returned here either, so a
    /// watcher built from this list never receives events for a genuinely ignored subtree in
    /// the first place — closing the gap `is_path_excluded`'s doc describes (a nested
    /// `.gitignore` between a root and a changed path forcing a full-crawl fallback), rather
    /// than reactively filtering events after they arrive.
    ///
    /// Symlinked directories are skipped (consistent with `follow_links(false)`): a watcher
    /// covering a symlink's target is a separate, deliberately unhandled case, not silently
    /// broken by this change (the pre-existing recursive-watch behaviour didn't traverse
    /// symlinks either, since none of the native backends follow them for a recursive watch).
    ///
    /// Stops as soon as more than `cap` directories are found (`WatchPlan::capped == true`,
    /// `dirs` empty) rather than enumerating a huge tree fully just to discard the result —
    /// the caller's job on a capped plan is to fall back to a coarser watch strategy, not to
    /// inspect which directories were found.
    pub fn plan_watch_dirs(
        root: &Path,
        matcher: &ExcludeMatcher,
        max_depth: Option<usize>,
        cap: usize,
    ) -> WatchPlan {
        let mut builder = WalkBuilder::new(root);
        builder.follow_links(false);
        builder.max_depth(Some(max_depth.unwrap_or(10)));
        builder.git_ignore(true);
        builder.hidden(false);
        builder.sort_by_file_name(std::ffi::OsStr::cmp);

        let root_for_filter = root.to_path_buf();
        let matcher_clone = matcher.clone();
        builder.filter_entry(move |entry| {
            let rel = match entry.path().strip_prefix(&root_for_filter) {
                Ok(r) => r,
                Err(_) => return true,
            };
            if rel.as_os_str().is_empty() {
                return true;
            }
            !matcher_clone.is_excluded_with_root(rel, Some(&root_for_filter))
        });

        let mut dirs = Vec::new();
        for result in builder.build() {
            let Ok(entry) = result else { continue };
            if entry.path_is_symlink() {
                continue;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                dirs.push(entry.path().to_path_buf());
                if dirs.len() > cap {
                    return WatchPlan {
                        dirs: Vec::new(),
                        capped: true,
                    };
                }
            }
        }
        WatchPlan {
            dirs,
            capped: false,
        }
    }
}

/// Result of [`FilesystemCrawler::plan_watch_dirs`]. `capped == true` means the tree has more
/// than the caller's `cap` watchable directories; `dirs` is empty in that case rather than a
/// truncated, misleading partial list — the caller's job is to pick a coarser watch strategy,
/// not to watch "some but not all" of a tree it couldn't fully enumerate within budget.
#[derive(Debug, Clone)]
pub struct WatchPlan {
    pub dirs: Vec<PathBuf>,
    pub capped: bool,
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

    #[test]
    fn test_exclude_matcher_root_prefixed_pattern() {
        let matcher = ExcludeMatcher::compile(&["**/deploy/submodules/**".to_string()]);
        let root = Path::new("/workspace/deploy");
        let rel = Path::new("submodules/config.yml");
        assert!(matcher.is_excluded_with_root(rel, Some(root)));
    }

    #[test]
    fn test_exclude_matcher_workspace_root_pattern() {
        let matcher = ExcludeMatcher::compile(&["${workspace_root}/docs/**".to_string()]);
        let rel = Path::new("docs/readme.md");
        assert!(matcher.is_excluded(rel));
    }

    #[test]
    fn test_is_path_excluded_matches_exclude_matcher() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir");
        let path = root.join("node_modules/pkg/index.js");
        std::fs::write(&path, "x").expect("write");

        let matcher = ExcludeMatcher::compile(&["node_modules".to_string()]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            Some(true)
        );
    }

    #[test]
    fn test_is_path_excluded_honors_root_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::write(root.join(".gitignore"), "*.generated.go\n").expect("write gitignore");
        let path = root.join("client.generated.go");
        std::fs::write(&path, "x").expect("write");

        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            Some(true),
            "root .gitignore pattern must be honored without a full crawl"
        );
    }

    #[test]
    fn test_is_path_excluded_kept_file_is_some_false() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        let path = root.join("src/main.rs");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, "fn main() {}").expect("write");

        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            Some(false)
        );
    }

    #[test]
    fn test_is_path_excluded_falls_back_on_nested_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::create_dir_all(root.join("services/billing")).expect("mkdir");
        // A .gitignore anywhere strictly between root and the file's own
        // directory is a nested one this fast check can't safely resolve.
        std::fs::write(root.join("services/.gitignore"), "*.tmp\n").expect("write");
        let path = root.join("services/billing/Widget.java");
        std::fs::write(&path, "class Widget {}").expect("write");

        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            None,
            "a nested .gitignore between root and the file must force a fall back, not a false negative"
        );
    }

    #[test]
    fn test_is_path_excluded_honors_root_ignore_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::write(root.join(".ignore"), "*.local.json\n").expect("write .ignore");
        let path = root.join("config.local.json");
        std::fs::write(&path, "{}").expect("write");

        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            Some(true),
            "root .ignore pattern must be honored, matching ignore::WalkBuilder's default ignore(true)"
        );
    }

    #[test]
    fn test_is_path_excluded_honors_git_info_exclude() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::create_dir_all(root.join(".git/info")).expect("mkdir");
        std::fs::write(root.join(".git/info/exclude"), "*.scratch\n").expect("write");
        let path = root.join("notes.scratch");
        std::fs::write(&path, "x").expect("write");

        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &path, &matcher),
            Some(true),
            "a repo-local .git/info/exclude pattern must be honored"
        );
    }

    #[test]
    fn test_is_path_excluded_root_itself_is_not_excluded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        let matcher = ExcludeMatcher::compile(&[]);
        assert_eq!(
            FilesystemCrawler::is_path_excluded(&root, &root, &matcher),
            Some(false),
            "path == root must short-circuit instead of walking a parent outside root"
        );
    }

    /// P2 step 3.3: an excluded directory (here, a nested `.gitignore`'s own rule, not just a
    /// static `exclude_patterns` entry) must never appear in the watch plan — this is the
    /// property `FileWatcherService` relies on to never register a watch inside it at all.
    #[test]
    fn plan_watch_dirs_excludes_gitignored_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        // `ignore::WalkBuilder`'s `git_ignore(true)` only honors `.gitignore` when
        // `require_git` (default true) finds a real `.git` directory — matching how a real
        // workspace actually looks, and the existing `.git/info/exclude` test's own fixture.
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(root.join("kept")).expect("mkdir kept");
        std::fs::create_dir_all(root.join("build/inner")).expect("mkdir build/inner");
        std::fs::write(root.join(".gitignore"), "build/\n").expect("write gitignore");

        let matcher = ExcludeMatcher::compile(&[]);
        let plan = FilesystemCrawler::plan_watch_dirs(&root, &matcher, Some(10), 100);
        assert!(!plan.capped);
        assert!(plan.dirs.iter().any(|d| d.ends_with("kept")));
        assert!(
            !plan.dirs.iter().any(|d| d.ends_with("build") || d.ends_with("inner")),
            "a gitignored directory (and its own contents) must never be planned for watching: {:?}",
            plan.dirs
        );
    }

    /// A nested `.gitignore` — strictly between `root` and the excluded directory, not `root`'s
    /// own top-level one — is exactly the case `is_path_excluded`'s doc says it cannot cheaply
    /// resolve for a single incoming path. `plan_watch_dirs` sidesteps that limitation entirely
    /// by walking (once, at watch-registration time) with the same `ignore::WalkBuilder` gitignore
    /// stacking a full crawl already gets right, rather than reproducing it per-event.
    #[test]
    fn plan_watch_dirs_excludes_nested_gitignore() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        std::fs::create_dir_all(root.join("service/build")).expect("mkdir");
        std::fs::write(root.join("service/.gitignore"), "build/\n")
            .expect("write nested gitignore");

        let matcher = ExcludeMatcher::compile(&[]);
        let plan = FilesystemCrawler::plan_watch_dirs(&root, &matcher, Some(10), 100);
        assert!(plan.dirs.iter().any(|d| d.ends_with("service")));
        assert!(
            !plan.dirs.iter().any(|d| d.ends_with("build")),
            "a directory excluded by a nested (non-root) .gitignore must not be planned either: {:?}",
            plan.dirs
        );
    }

    #[test]
    fn plan_watch_dirs_reports_capped_without_a_partial_list() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(temp.path()).expect("canon");
        for i in 0..10 {
            std::fs::create_dir_all(root.join(format!("d{i}"))).expect("mkdir");
        }
        let matcher = ExcludeMatcher::compile(&[]);
        // root itself + 10 subdirectories = 11 directories; cap at 3 must trip.
        let plan = FilesystemCrawler::plan_watch_dirs(&root, &matcher, Some(10), 3);
        assert!(plan.capped);
        assert!(
            plan.dirs.is_empty(),
            "a capped plan must not return a partial, misleading directory list"
        );
    }
}
