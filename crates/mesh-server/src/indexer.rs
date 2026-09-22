//! Workspace indexing shared by `mesh-mcp run`, `mesh-mcp graph`, `meshd` and the
//! file watcher. Previously each of those carried its own copy of the scan loop.
//!
//! Extraction (read + guard + tree-sitter) runs in parallel on the QoS-throttled
//! Rayon pool and produces graph-independent [`FileIndex`] fragments; only the
//! final fold into the `ContractGraph` and the single `reconcile_edges` pass are
//! sequential.

use mesh_core::{
    expand_roots, AppState, BackgroundRescanEngine, Config, ContractGraph, DifferentialVfs,
    DocIndex, DocSection, FilesystemCrawler, MeshSnapshot, PropertyRegistry, RepoId,
    ValidatedScope,
};
use mesh_parsers::{AstGuard, CompiledPattern, FileIndex, LanguageKind, PolyglotIndexer};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Crawl depth used for every full workspace scan.
pub const SCAN_DEPTH: usize = 10;

/// Output of processing one file on the pool.
struct FileFragment {
    path: PathBuf,
    signature: mesh_core::vfs::FileSignature,
    code: FileIndex,
    docs: Vec<DocSection>,
    props: Option<PropertyRegistry>,
}

pub struct WorkspaceIndexer;

impl WorkspaceIndexer {
    // ── Configuration discovery ─────────────────────────────────────────────

    /// Loads the first config found among `explicit`, `.agents/mesh-mcp.toml`,
    /// `mesh-mcp.toml`, falling back to a single-root default. Returns the config
    /// and the directory relative roots are resolved against.
    pub fn discover_config(
        explicit: Option<&Path>,
    ) -> Result<(Config, PathBuf), Box<dyn std::error::Error>> {
        let candidates = [
            explicit.map(Path::to_path_buf),
            Some(PathBuf::from(".agents/mesh-mcp.toml")),
            Some(PathBuf::from("mesh-mcp.toml")),
        ];
        match candidates.into_iter().flatten().find(|p| p.exists()) {
            Some(path) => {
                let mut cfg = Config::load_from_file(&path)?;
                let base = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                // Skill paths are written relative to the config file; the server is
                // spawned by an IDE with an arbitrary cwd.
                cfg.resolve_skill_paths(&base);
                Ok((cfg, base))
            }
            None => {
                let default = concat!(
                    "[workspace]\nname = \"default-mesh\"\nversion = \"",
                    env!("CARGO_PKG_VERSION"),
                    "\"\nroots = [\".\"]\n"
                );
                Ok((Config::load_from_str(default)?, PathBuf::from(".")))
            }
        }
    }

    /// Expands configured roots; on failure falls back to `base_dir` with a warning.
    pub fn resolve_roots(config: &Config, base_dir: &Path) -> Vec<PathBuf> {
        match expand_roots(
            &config.workspace.roots,
            base_dir,
            &config.workspace.workspace_root,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "mesh::config",
                    "Failed to expand roots: {e}. Falling back to base directory."
                );
                vec![dunce::canonicalize(base_dir).unwrap_or_else(|_| base_dir.to_path_buf())]
            }
        }
    }

    /// Display names for each root, indexed by `RepoId`.
    pub fn repo_names(roots: &[PathBuf]) -> Vec<String> {
        roots
            .iter()
            .map(|r| {
                r.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| r.display().to_string())
            })
            .collect()
    }

    // ── Full scan ───────────────────────────────────────────────────────────

    /// Scans every root and builds a fresh snapshot. `pool` selects the Rayon pool:
    /// pass `None` (global pool) for the boot scan — the caller is waiting and the
    /// QoS-background engine runs ~5x slower on Apple Silicon E-cores — and the
    /// throttled engine for reloads behind a live IDE. `vfs`, if given, is seeded with
    /// the signatures of every indexed file so the first incremental reload is cheap.
    pub fn build_snapshot(
        config: &Config,
        roots: &[PathBuf],
        pool: Option<&BackgroundRescanEngine>,
        vfs: Option<&mut DifferentialVfs>,
    ) -> MeshSnapshot {
        let files = Self::crawl_all(config, roots);
        let patterns = Self::compiled_patterns(config);
        let doc_template = DocIndex::default();

        let work = || {
            files
                .par_iter()
                .filter_map(|(repo_id, path)| {
                    Self::process_file(path, *repo_id, &patterns, &doc_template)
                })
                .collect::<Vec<_>>()
        };
        let fragments = match pool {
            Some(engine) => engine.install(work),
            None => work(),
        };

        let mut snapshot = MeshSnapshot::default();
        let mut vfs = vfs;
        for frag in fragments {
            if let Some(vfs) = vfs.as_deref_mut() {
                vfs.upsert(&frag.path, frag.signature.clone());
            }
            Self::fold(frag, &mut snapshot);
        }
        snapshot.contract_graph.reconcile_edges();

        tracing::info!(
            target: "mesh::indexer",
            "Workspace scan complete: {} files, {} contract nodes, {} edges, {} doc sections.",
            files.len(),
            snapshot.contract_graph.node_count(),
            snapshot.contract_graph.edge_count(),
            snapshot.doc_index.section_count()
        );
        snapshot
    }

    /// Convenience for the CLI: graph only, using the global Rayon pool.
    pub fn build_graph(config: &Config, roots: &[PathBuf]) -> (ContractGraph, usize) {
        let files = Self::crawl_all(config, roots);
        let patterns = Self::compiled_patterns(config);
        let doc_template = DocIndex::default();
        let mut graph = ContractGraph::new();
        let fragments: Vec<_> = files
            .par_iter()
            .filter_map(|(repo_id, path)| {
                Self::process_file(path, *repo_id, &patterns, &doc_template)
            })
            .collect();
        for frag in fragments {
            frag.code.apply(&mut graph);
        }
        graph.reconcile_edges();
        (graph, files.len())
    }

    // ── Incremental reload ──────────────────────────────────────────────────

    /// Differential reload driven by the state's VFS: only files whose stat or
    /// content hash changed are re-read and re-indexed; deleted files are purged.
    /// Installs one consistent snapshot at the end.
    pub fn reload(state: &AppState) {
        let config = &state.config;
        let files = Self::crawl_all(config, &state.allowed_roots);
        let patterns = Self::compiled_patterns(config);

        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());

        // Deleted: tracked by the VFS but no longer on disk.
        let present: HashSet<&Path> = files.iter().map(|(_, p)| p.as_path()).collect();
        let deleted: Vec<PathBuf> = vfs
            .tracked_paths()
            .filter(|p| !present.contains(p))
            .map(Path::to_path_buf)
            .collect();

        // Candidates: new or stat-changed. The metadata call is the only I/O for
        // unchanged files — they are never read.
        let candidates: Vec<(RepoId, &PathBuf)> = files
            .iter()
            .filter(|(_, p)| match std::fs::metadata(p) {
                Ok(m) => !vfs.is_unchanged_fast(p, &m),
                Err(_) => false,
            })
            .map(|(r, p)| (*r, p))
            .collect();

        if candidates.is_empty() && deleted.is_empty() {
            tracing::debug!(target: "mesh::watcher", "VFS differential: 0 files changed, skipping reload.");
            return;
        }

        let doc_template = state.snapshot().doc_index.clone_settings();
        let fragments: Vec<FileFragment> = state.rescan.install(|| {
            candidates
                .par_iter()
                .filter_map(|(repo_id, path)| {
                    Self::process_file(path, *repo_id, &patterns, &doc_template)
                })
                .collect()
        });

        // Bare `touch`es hash identical and are dropped here.
        let changed: Vec<FileFragment> = fragments
            .into_iter()
            .filter(|f| vfs.upsert(&f.path, f.signature.clone()))
            .collect();
        for path in &deleted {
            vfs.remove(path);
        }
        drop(vfs);

        if changed.is_empty() && deleted.is_empty() {
            return;
        }
        if changed.len() > 200 {
            tracing::info!(
                target: "mesh::watcher",
                "Mass filesystem mutation detected ({} files). Running full differential rebuild.",
                changed.len()
            );
        }

        let mut snapshot = state.snapshot_clone();
        let stale = changed
            .iter()
            .map(|f| f.path.as_path())
            .chain(deleted.iter().map(PathBuf::as_path));
        snapshot.contract_graph.patch_files(stale);
        for f in &changed {
            snapshot.doc_index.remove_file(&f.path);
        }
        for p in &deleted {
            snapshot.doc_index.remove_file(p);
        }
        let changed_count = changed.len();
        for frag in changed {
            Self::fold(frag, &mut snapshot);
        }
        snapshot.contract_graph.reconcile_edges();
        let node_count = snapshot.contract_graph.node_count();
        let generation = state.install_snapshot(snapshot);

        tracing::info!(
            target: "mesh::watcher",
            "Incremental reload (gen {generation}): {changed_count} re-indexed, {} removed, {node_count} contract nodes.",
            deleted.len()
        );
    }

    // ── Internals ───────────────────────────────────────────────────────────

    fn compiled_patterns(config: &Config) -> Vec<CompiledPattern> {
        config
            .engines
            .contracts
            .as_ref()
            .map(|c| CompiledPattern::compile_all(&c.patterns))
            .unwrap_or_default()
    }

    /// Crawls all roots, tagging each file with the `RepoId` of its root.
    fn crawl_all(config: &Config, roots: &[PathBuf]) -> Vec<(RepoId, PathBuf)> {
        let mut out = Vec::new();
        for (idx, root) in roots.iter().enumerate() {
            let repo_id = idx as RepoId;
            match ValidatedScope::resolve(&root.to_string_lossy(), roots) {
                Ok(scope) => {
                    let files = FilesystemCrawler::crawl_scope(
                        &scope,
                        &config.workspace.exclude_patterns,
                        Some(SCAN_DEPTH),
                    );
                    out.extend(files.into_iter().map(|f| (repo_id, f)));
                }
                Err(e) => {
                    tracing::warn!(target: "mesh::indexer", "Skipping root {}: {e}", root.display());
                }
            }
        }
        out
    }

    /// Reads, guards and extracts one file. Runs on the pool; touches no shared state.
    fn process_file(
        path: &Path,
        repo_id: RepoId,
        patterns: &[CompiledPattern],
        doc_template: &DocIndex,
    ) -> Option<FileFragment> {
        let metadata = std::fs::metadata(path).ok()?;
        // Commandment 2: check the size budget *before* reading, so an oversized file
        // never costs its full read.
        if !AstGuard::within_size_budget(path, &metadata) {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let path_str = path.to_string_lossy();
        let lang = LanguageKind::from_path(&path_str);

        // Tree-sitter inputs get the full lexical guard (null bytes, line length,
        // nesting). Markdown / YAML / properties only get the binary sniff — prose
        // legitimately has >1 KB lines.
        let passes = if lang.language().is_some() {
            AstGuard::should_parse_path(path, &metadata, &bytes)
        } else {
            !AstGuard::looks_binary(&bytes)
        };
        if !passes {
            return None;
        }
        let content = std::str::from_utf8(&bytes).ok()?;
        let signature = DifferentialVfs::compute_signature(&metadata, &bytes);

        let mut frag = FileFragment {
            path: path.to_path_buf(),
            signature,
            code: FileIndex::default(),
            docs: Vec::new(),
            props: None,
        };

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "md" => frag.docs = doc_template.parse_sections(path, content),
            "properties" => {
                let mut reg = PropertyRegistry::new();
                reg.ingest_properties_str(content);
                frag.props = Some(reg);
            }
            "yml" | "yaml" => {
                let mut reg = PropertyRegistry::new();
                let _ = reg.ingest_yaml_str(content);
                frag.props = Some(reg);
                frag.code = PolyglotIndexer::extract(path, content, repo_id);
            }
            _ => frag.code = PolyglotIndexer::extract(path, content, repo_id),
        }
        if !patterns.is_empty() {
            frag.code.merge(PolyglotIndexer::extract_custom_patterns(
                path, content, repo_id, patterns,
            ));
        }

        Some(frag)
    }

    fn fold(frag: FileFragment, snapshot: &mut MeshSnapshot) {
        frag.code.apply(&mut snapshot.contract_graph);
        if !frag.docs.is_empty() {
            snapshot.doc_index.extend_sections(frag.docs);
        }
        if let Some(props) = frag.props {
            snapshot.property_registry.merge(props);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_core::{AuditLogger, Config};
    use std::sync::Arc;

    fn make_state(root: &Path) -> Arc<AppState> {
        let cfg =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        Arc::new(AppState::new(cfg, vec![root.to_path_buf()], audit, rescan))
    }

    #[test]
    fn full_scan_then_differential_reload_handles_edit_and_delete() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(
            root.join("a.proto"),
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); }",
        )
        .expect("write a");
        std::fs::write(
            root.join("b.proto"),
            "syntax = \"proto3\"; package b; service B { rpc Y (R) returns (S); }",
        )
        .expect("write b");
        std::fs::write(root.join("doc.md"), "# Title\nsome text").expect("write md");

        let state = make_state(&root);
        let snap = {
            let mut vfs = state.vfs.lock().expect("vfs");
            WorkspaceIndexer::build_snapshot(
                &state.config,
                &state.allowed_roots,
                Some(&state.rescan),
                Some(&mut vfs),
            )
        };
        state.install_snapshot(snap);
        assert_eq!(state.snapshot().contract_graph.node_count(), 4);
        assert_eq!(state.snapshot().doc_index.section_count(), 1);
        assert_eq!(state.vfs.lock().expect("vfs").len(), 3);

        // Nothing changed → no new generation.
        WorkspaceIndexer::reload(&state);
        assert_eq!(state.snapshot().generation, 1);

        // Edit one file, delete another.
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(
            root.join("a.proto"),
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); rpc Z (R) returns (S); }",
        )
        .expect("rewrite a");
        std::fs::remove_file(root.join("b.proto")).expect("rm b");

        WorkspaceIndexer::reload(&state);
        let view = state.snapshot();
        assert_eq!(view.generation, 2);
        // a: service + 2 rpcs = 3 nodes; b gone.
        assert_eq!(view.contract_graph.node_count(), 3);
        assert!(view.contract_graph.search_symbols("B", None).is_empty());
        assert_eq!(state.vfs.lock().expect("vfs").len(), 2);
    }
}
