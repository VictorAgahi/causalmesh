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
        let walker = builder.build();

        for result in walker {
            match result {
                Ok(entry) => {
                    let path = entry.path();
                    if entry.file_type().is_some_and(|ft| ft.is_file())
                        && !Self::is_excluded(path, exclude_patterns)
                    {
                        files.push(path.to_path_buf());
                    }
                }
                Err(err) => {
                    tracing::debug!(target: "mesh::crawler", "Skipping inaccessible entry: {err}");
                }
            }
        }

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
}
