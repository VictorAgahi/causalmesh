use crate::contracts::ContractGraph;
use crate::crawler::FilesystemCrawler;
use crate::security::ValidatedScope;
use crate::state::AppState;
use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// In-kernel filesystem watcher service providing debounced change notifications
/// and atomic ArcSwap reloading per RFC-001 Commandment 7.
pub struct FileWatcherService;

impl FileWatcherService {
    pub const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(150);

    /// Spawns the debounced file watcher actor in a background thread
    pub fn spawn<F>(
        state: Arc<AppState>,
        cancel_token: CancellationToken,
        indexer: F,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(&Path, &str, &mut ContractGraph) + Send + Sync + 'static,
    {
        let indexer = Arc::new(indexer);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Self::DEBOUNCE_INTERVAL, tx)?;

        let allowed_roots = state.allowed_roots.load().clone();
        for root in allowed_roots.as_ref() {
            if root.exists() {
                debouncer.watcher().watch(root, RecursiveMode::Recursive)?;
                tracing::info!(target: "mesh::watcher", "FileWatcher watching root: {}", root.display());

                // Watch .git/HEAD for branch checkouts / rebases
                let git_head = root.join(".git").join("HEAD");
                if git_head.exists() {
                    let _ = debouncer
                        .watcher()
                        .watch(&git_head, RecursiveMode::NonRecursive);
                }
            }
        }

        let handle = std::thread::Builder::new()
            .name("mesh-file-watcher".to_string())
            .spawn(move || {
                let _watcher = debouncer;

                loop {
                    if cancel_token.is_cancelled() {
                        tracing::info!(target: "mesh::watcher", "FileWatcher received cancellation signal, stopping.");
                        break;
                    }

                    match rx.recv_timeout(Duration::from_millis(300)) {
                        Ok(event_res) => match event_res {
                            Ok(events) => {
                                let relevant = events.iter().any(|ev| Self::is_relevant_path(&ev.path));
                                if relevant {
                                    tracing::info!(
                                        target: "mesh::watcher",
                                        "Detected filesystem mutations ({} events). Scheduling atomic graph rescan.",
                                        events.len()
                                    );
                                    Self::schedule_reload(state.clone(), indexer.clone());
                                }
                            }
                            Err(errs) => {
                                tracing::warn!(target: "mesh::watcher", "File watch error: {:?}", errs);
                            }
                        },
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;

        Ok(handle)
    }

    /// Determines if a changed path is relevant for index rebuilding
    pub fn is_relevant_path(path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        if path_str.starts_with("target/")
            || path_str.contains("/target/")
            || path_str.starts_with("node_modules/")
            || path_str.contains("/node_modules/")
            || path_str.starts_with(".git/objects/")
            || path_str.contains("/.git/objects/")
            || path_str.ends_with(".swp")
            || path_str.ends_with('~')
            || path_str.ends_with(".tmp")
            || path_str.ends_with(".lock")
        {
            return false;
        }

        if path_str.ends_with(".git/HEAD")
            || path_str.ends_with("/.git/HEAD")
            || path_str.contains(".git/refs/")
        {
            return true;
        }

        path_str.ends_with(".rs")
            || path_str.ends_with(".go")
            || path_str.ends_with(".java")
            || path_str.ends_with(".ts")
            || path_str.ends_with(".tsx")
            || path_str.ends_with(".js")
            || path_str.ends_with(".py")
            || path_str.ends_with(".proto")
            || path_str.ends_with(".yaml")
            || path_str.ends_with(".yml")
            || path_str.ends_with(".properties")
            || path_str.ends_with(".md")
    }

    /// Reloads the polyglot contract graph, doc index, and property registry on the background Rayon pool.
    /// Uses DifferentialVfs to avoid re-parsing files whose content hasn't changed.
    pub fn schedule_reload<F>(state: Arc<AppState>, indexer: Arc<F>)
    where
        F: Fn(&Path, &str, &mut ContractGraph) + Send + Sync + 'static,
    {
        let state_clone = state.clone();
        std::mem::drop(state.rescan.spawn(move || {
            Self::execute_reload_sync(&state_clone, indexer.as_ref());
        }));
    }

    /// Synchronous rescan logic executed on the QoS-throttled thread pool.
    /// Performs a differential reload: only files with changed content hashes are re-indexed.
    /// For massive filesystem mutations (>200 files changed, e.g. `git checkout`),
    /// processes files in throttled batches to prevent I/O thrashing and IDE stuttering.
    pub fn execute_reload_sync<F>(state: &AppState, indexer: &F)
    where
        F: Fn(&Path, &str, &mut ContractGraph),
    {
        let allowed_roots = state.allowed_roots.load().clone();
        let exclude_patterns = state.config.load().workspace.exclude_patterns.clone();

        // Collect all files across all roots
        let mut all_files = Vec::new();
        for root in allowed_roots.iter() {
            if let Ok(validated_scope) =
                ValidatedScope::resolve(&root.to_string_lossy(), &allowed_roots)
            {
                let files =
                    FilesystemCrawler::crawl_scope(&validated_scope, &exclude_patterns, Some(10));
                all_files.extend(files);
            }
        }

        // Use a thread-local VFS cache to avoid re-parsing unchanged files.
        // For the first run, all files are considered changed.
        use std::sync::Mutex;
        use once_cell::sync::Lazy;
        static VFS_CACHE: Lazy<Mutex<crate::vfs::DifferentialVfs>> =
            Lazy::new(|| Mutex::new(crate::vfs::DifferentialVfs::new()));

        let mut vfs = VFS_CACHE.lock().unwrap_or_else(|e| e.into_inner());

        let mut changed_files: Vec<(std::path::PathBuf, String)> = Vec::new();
        for file in &all_files {
            if let Ok(content) = std::fs::read_to_string(file) {
                if vfs.check_and_update(file, &content) {
                    changed_files.push((file.clone(), content));
                }
            }
        }

        if changed_files.is_empty() {
            tracing::debug!(
                target: "mesh::watcher",
                "VFS differential: 0 files changed, skipping reload."
            );
            return;
        }

        let is_mass_checkout = changed_files.len() > 200;
        if is_mass_checkout {
            tracing::info!(
                target: "mesh::watcher",
                "Mass filesystem mutation detected ({} files). Running throttled full rebuild.",
                changed_files.len()
            );
        }

        // Clone the current graph and apply incremental patches
        let current_graph = state.contract_graph.load();
        let mut new_graph = (**current_graph).clone();
        let mut new_doc_index = (**state.doc_index.load()).clone();
        let mut new_prop_reg = (**state.property_registry.load()).clone();

        for (batch_idx, (file, content)) in changed_files.iter().enumerate() {
            // Throttle I/O on massive checkouts to preserve IDE responsiveness
            if is_mass_checkout && batch_idx % 50 == 49 {
                std::thread::yield_now();
            }

            // Purge stale nodes for this file path before re-indexing
            new_graph.patch_file(file);

            let path_str = file.to_string_lossy();
            if path_str.ends_with(".md") {
                new_doc_index.index_markdown_file(file, content);
            } else if path_str.ends_with(".properties") {
                new_prop_reg.ingest_properties_str(content);
            } else if path_str.ends_with(".yml") || path_str.ends_with(".yaml") {
                let _ = new_prop_reg.ingest_yaml_str(content);
                indexer(file, content, &mut new_graph);
            } else {
                indexer(file, content, &mut new_graph);
            }
        }

        state.contract_graph.store(Arc::new(new_graph));
        state.doc_index.store(Arc::new(new_doc_index));
        state.property_registry.store(Arc::new(new_prop_reg));

        tracing::info!(
            target: "mesh::watcher",
            "Incremental reload completed: {}/{} files re-indexed, {} total contract nodes.",
            changed_files.len(),
            all_files.len(),
            state.contract_graph.load().node_count()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relevant_paths_filter() {
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "src/main.rs"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "api/auth.proto"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            "service.ts"
        )));
        assert!(FileWatcherService::is_relevant_path(Path::new(".git/HEAD")));
        assert!(FileWatcherService::is_relevant_path(Path::new(
            ".git/refs/heads/main"
        )));

        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "target/debug/app"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "node_modules/pkg/index.js"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            ".git/objects/4b/825dc"
        )));
        assert!(!FileWatcherService::is_relevant_path(Path::new(
            "src/main.rs.swp"
        )));
    }
}
