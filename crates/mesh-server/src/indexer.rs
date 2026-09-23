//! Workspace indexing shared by `mesh-mcp run`, `mesh-mcp graph`, `meshd` and the
//! file watcher. Previously each of those carried its own copy of the scan loop.
//!
//! Extraction (read + guard + tree-sitter) runs in parallel on the QoS-throttled
//! Rayon pool and produces graph-independent [`FileIndex`] fragments; only the
//! final fold into the `ContractGraph` and the single `reconcile_edges` pass are
//! sequential.

use mesh_core::{
    expand_roots, AppState, BackgroundRescanEngine, Config, ContractGraph, DifferentialVfs,
    DocIndex, DocSection, ExcludeMatcher, FilesystemCrawler, MeshSnapshot, PropertyRegistry,
    PropertySourceMatcher, RepoId, ValidatedScope,
};
use mesh_parsers::{
    AstGuard, CompiledPattern, ExtractConfig, FileIndex, LanguageKind, PolyglotIndexer,
};
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

/// `[engines.contracts.spring]` settings resolved once per scan, mirroring how
/// `doc_index_for`/`compiled_patterns` resolve their own engine's config once instead of per
/// file. Absent `[engines.contracts.spring]` keeps the pre-existing unscoped, redacting,
/// non-resolving behaviour.
#[derive(Clone)]
struct SpringSettings {
    /// `None` means "no scoping" (match every `.properties`/`.yml`/`.yaml` file), which is both
    /// the default and the behaviour when `property_files` is left empty.
    file_matcher: Option<PropertySourceMatcher>,
    resolve_placeholders: bool,
    auto_redact_secrets: bool,
}

impl SpringSettings {
    fn from_config(config: &Config) -> Self {
        match config
            .engines
            .contracts
            .as_ref()
            .and_then(|c| c.spring.as_ref())
        {
            Some(spring) => Self {
                file_matcher: if spring.property_files.is_empty() {
                    None
                } else {
                    Some(PropertySourceMatcher::compile(&spring.property_files))
                },
                resolve_placeholders: spring.resolve_placeholders,
                auto_redact_secrets: spring.auto_redact_secrets,
            },
            None => Self {
                file_matcher: None,
                resolve_placeholders: true,
                auto_redact_secrets: true,
            },
        }
    }

    #[inline]
    fn is_property_source(&self, path: &Path) -> bool {
        match &self.file_matcher {
            Some(matcher) => matcher.is_match(path),
            None => true,
        }
    }
}

/// Per-scan snapshot of which engines are switched on, plus the compiled
/// `[engines.docs] paths` allowlist. Built once per scan from `Config` so
/// `enabled = false` and `paths` actually change what gets indexed instead of
/// being accepted-but-ignored config keys.
struct EngineToggles {
    docs_enabled: bool,
    contracts_enabled: bool,
    /// `None` means `[engines.docs] paths` is empty: no restriction, every `.md`
    /// file under the crawled roots is indexed (the historical behaviour).
    doc_paths: Option<ExcludeMatcher>,
}

/// Everything `process_file` needs beyond the file's own path/repo_id/root,
/// resolved once per scan and bundled to keep the function's argument count
/// down (clippy::too_many_arguments).
struct ScanConfig<'a> {
    patterns: &'a [CompiledPattern],
    doc_template: &'a DocIndex,
    spring: &'a SpringSettings,
    extract_cfg: &'a ExtractConfig,
    toggles: &'a EngineToggles,
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
        let doc_template = Self::doc_index_for(config);
        let spring = SpringSettings::from_config(config);
        let extract_cfg = Self::extract_config(config);
        let toggles = Self::engine_toggles(config);
        let scan_cfg = ScanConfig {
            patterns: &patterns,
            doc_template: &doc_template,
            spring: &spring,
            extract_cfg: &extract_cfg,
            toggles: &toggles,
        };

        let work = || {
            files
                .par_iter()
                .filter_map(|(repo_id, path)| {
                    let root = roots
                        .get(*repo_id as usize)
                        .map_or(path.as_path(), |r| r.as_path());
                    Self::process_file(path, *repo_id, root, &scan_cfg)
                })
                .collect::<Vec<_>>()
        };
        let fragments = match pool {
            Some(engine) => engine.install(work),
            None => work(),
        };

        let mut snapshot = MeshSnapshot {
            doc_index: Self::doc_index_for(config),
            ..MeshSnapshot::default()
        };
        let mut vfs = vfs;
        for frag in fragments {
            if let Some(vfs) = vfs.as_deref_mut() {
                vfs.upsert(&frag.path, frag.signature.clone());
            }
            Self::fold(frag, &mut snapshot);
        }
        if spring.resolve_placeholders {
            snapshot.property_registry.resolve_all_placeholders();
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
        let doc_template = Self::doc_index_for(config);
        let spring = SpringSettings::from_config(config);
        let extract_cfg = Self::extract_config(config);
        let toggles = Self::engine_toggles(config);
        let scan_cfg = ScanConfig {
            patterns: &patterns,
            doc_template: &doc_template,
            spring: &spring,
            extract_cfg: &extract_cfg,
            toggles: &toggles,
        };
        let mut graph = ContractGraph::new();
        let fragments: Vec<_> = files
            .par_iter()
            .filter_map(|(repo_id, path)| {
                let root = roots
                    .get(*repo_id as usize)
                    .map_or(path.as_path(), |r| r.as_path());
                Self::process_file(path, *repo_id, root, &scan_cfg)
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
        let roots = &state.allowed_roots;
        let files = Self::crawl_all(config, roots);
        let patterns = Self::compiled_patterns(config);
        let spring = SpringSettings::from_config(config);
        let extract_cfg = Self::extract_config(config);
        let toggles = Self::engine_toggles(config);

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
        let scan_cfg = ScanConfig {
            patterns: &patterns,
            doc_template: &doc_template,
            spring: &spring,
            extract_cfg: &extract_cfg,
            toggles: &toggles,
        };
        let fragments: Vec<FileFragment> = state.rescan.install(|| {
            candidates
                .par_iter()
                .filter_map(|(repo_id, path)| {
                    let root = roots
                        .get(*repo_id as usize)
                        .map_or(path.as_path(), |r| r.as_path());
                    Self::process_file(path, *repo_id, root, &scan_cfg)
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
            snapshot.property_registry.remove_file(&f.path);
        }
        for p in &deleted {
            snapshot.doc_index.remove_file(p);
            snapshot.property_registry.remove_file(p);
        }
        let changed_count = changed.len();
        for frag in changed {
            Self::fold(frag, &mut snapshot);
        }
        if spring.resolve_placeholders {
            snapshot.property_registry.resolve_all_placeholders();
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

    /// Builds the doc index from `[engines.docs]` so aliases, stop words and the
    /// phrase boost configured by the user actually apply. A default-constructed
    /// `DocIndex` silently ignores all of them.
    fn doc_index_for(config: &Config) -> DocIndex {
        match config.engines.docs.as_ref() {
            Some(docs) => DocIndex::new(
                docs.aliases.clone(),
                docs.stop_words.clone(),
                docs.exact_phrase_boost,
                docs.sanitize_prompt_injections,
                docs.fuzzy_fallback,
            ),
            None => DocIndex::default(),
        }
    }

    fn compiled_patterns(config: &Config) -> Vec<CompiledPattern> {
        config
            .engines
            .contracts
            .as_ref()
            .map(|c| CompiledPattern::compile_all(&c.patterns))
            .unwrap_or_default()
    }

    /// Builds the extraction knobs from `[engines.contracts.grpc]` /
    /// `.openapi` / `.asyncapi` so `proto_dirs`, `controller_annotations`,
    /// `canonical_fqcn_projection`, `spec_files` and `infer_string_topics`
    /// actually apply instead of being parsed and ignored.
    fn extract_config(config: &Config) -> ExtractConfig {
        config
            .engines
            .contracts
            .as_ref()
            .map(ExtractConfig::from_contracts)
            .unwrap_or_default()
    }

    /// Reads `[engines.docs] enabled`, `[engines.contracts] enabled` and
    /// `[engines.docs] paths` from config. Absent sections default to enabled
    /// (matching each config struct's own `#[serde(default = "default_true")]`),
    /// and an empty `paths` list means "no restriction".
    fn engine_toggles(config: &Config) -> EngineToggles {
        let docs_enabled = config.engines.docs.as_ref().is_none_or(|d| d.enabled);
        let contracts_enabled = config.engines.contracts.as_ref().is_none_or(|c| c.enabled);
        let doc_paths = config
            .engines
            .docs
            .as_ref()
            .filter(|d| !d.paths.is_empty())
            .map(|d| ExcludeMatcher::compile(&d.paths));

        EngineToggles {
            docs_enabled,
            contracts_enabled,
            doc_paths,
        }
    }

    /// Crawls all roots, tagging each file with the `RepoId` of its root.
    fn crawl_all(config: &Config, roots: &[PathBuf]) -> Vec<(RepoId, PathBuf)> {
        if roots.len() > RepoId::MAX as usize {
            tracing::error!(
                target: "mesh::indexer",
                "Root count ({}) exceeds maximum supported RepoId limit ({})",
                roots.len(),
                RepoId::MAX
            );
        }
        let mut out = Vec::new();
        for (idx, root) in roots.iter().enumerate().take(RepoId::MAX as usize) {
            let repo_id = idx as RepoId;
            match ValidatedScope::resolve_with_aliases(
                &root.to_string_lossy(),
                roots,
                &config.workspace.mount_aliases,
            ) {
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
        root: &Path,
        cfg: &ScanConfig,
    ) -> Option<FileFragment> {
        let ScanConfig {
            patterns,
            doc_template,
            spring,
            extract_cfg,
            toggles,
        } = *cfg;
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
            "md" => {
                if toggles.docs_enabled && Self::doc_path_allowed(path, root, &toggles.doc_paths) {
                    frag.docs = doc_template.parse_sections(path, content);
                }
            }
            "properties" => {
                if spring.is_property_source(path) {
                    let mut reg = PropertyRegistry::with_redaction(spring.auto_redact_secrets);
                    reg.ingest_properties_str(content);
                    frag.props = Some(reg);
                }
            }
            "yml" | "yaml" => {
                if spring.is_property_source(path) {
                    let mut reg = PropertyRegistry::with_redaction(spring.auto_redact_secrets);
                    let _ = reg.ingest_yaml_str(content);
                    frag.props = Some(reg);
                }
                if toggles.contracts_enabled {
                    frag.code =
                        PolyglotIndexer::extract_with_config(path, content, repo_id, extract_cfg);
                }
            }
            _ => {
                if toggles.contracts_enabled {
                    frag.code =
                        PolyglotIndexer::extract_with_config(path, content, repo_id, extract_cfg)
                }
            }
        }
        if toggles.contracts_enabled && !patterns.is_empty() {
            frag.code.merge(PolyglotIndexer::extract_custom_patterns(
                path, content, repo_id, patterns,
            ));
        }

        Some(frag)
    }

    /// `paths` is empty (default) → no restriction. Otherwise the file's path,
    /// relative to its own repo root, must match one of the configured globs
    /// (e.g. `"docs/**"`, `"*.md"`). A file outside `root` (shouldn't happen —
    /// the crawl is rooted there) is conservatively allowed.
    fn doc_path_allowed(path: &Path, root: &Path, doc_paths: &Option<ExcludeMatcher>) -> bool {
        let Some(matcher) = doc_paths else {
            return true;
        };
        match path.strip_prefix(root) {
            Ok(rel) => matcher.is_excluded(rel),
            Err(_) => true,
        }
    }

    fn fold(frag: FileFragment, snapshot: &mut MeshSnapshot) {
        if !frag.docs.is_empty() {
            snapshot.doc_index.extend_sections(frag.docs);
        }
        if let Some(props) = frag.props {
            snapshot.property_registry.merge(props, &frag.path);
        }
        frag.code.apply(&mut snapshot.contract_graph);
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

    /// Item 10: `PropertyRegistry` provenance. Deleting one of two properties files must remove
    /// exactly its keys on the next incremental reload, leaving the other file's keys intact —
    /// previously `merge` only ever added, so deleted keys persisted until a full restart.
    #[test]
    fn reload_removes_deleted_properties_files_keys_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(root.join("a.properties"), "app.a.name=a-value\n").expect("write a");
        std::fs::write(root.join("b.properties"), "app.b.name=b-value\n").expect("write b");

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
        assert_eq!(
            state.snapshot().property_registry.get("app.a.name"),
            Some("a-value")
        );
        assert_eq!(
            state.snapshot().property_registry.get("app.b.name"),
            Some("b-value")
        );

        std::fs::remove_file(root.join("b.properties")).expect("rm b");
        WorkspaceIndexer::reload(&state);

        let view = state.snapshot();
        assert_eq!(view.generation, 2);
        assert_eq!(view.property_registry.get("app.a.name"), Some("a-value"));
        assert_eq!(view.property_registry.get("app.b.name"), None);
    }

    /// Item 1: `[engines.contracts.spring]` `property_files` / `auto_redact_secrets` /
    /// `resolve_placeholders` must produce observably different behaviour from the (implicit)
    /// defaults exercised by the other tests in this module.
    #[test]
    fn spring_config_keys_change_scan_behaviour() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(
            root.join("application.properties"),
            "app.secret.token=raw_token_value\napp.greeting=${app.name:World}\n",
        )
        .expect("write application.properties");
        std::fs::write(root.join("other.properties"), "other.key=other-value\n")
            .expect("write other.properties");

        let cfg = Config::load_from_str(
            r#"
[workspace]
name = "t"
version = "0"
roots = ["."]

[engines.contracts.spring]
property_files = ["application*.properties"]
auto_redact_secrets = false
resolve_placeholders = true
"#,
        )
        .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, vec![root.clone()], audit, rescan));

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
        let view = state.snapshot();

        // property_files scoped to application*.properties: other.properties is not ingested.
        assert_eq!(view.property_registry.get("other.key"), None);
        // auto_redact_secrets = false: a key that would normally be masked stays raw.
        assert_eq!(
            view.property_registry.get("app.secret.token"),
            Some("raw_token_value")
        );
        // resolve_placeholders = true: the ${app.name:World} default is resolved in place.
        assert_eq!(view.property_registry.get("app.greeting"), Some("World"));
    }

    /// `[engines.docs] enabled = false` must stop `.md` files from being indexed
    /// at all, instead of the flag being accepted-but-ignored.
    #[test]
    fn docs_engine_disabled_skips_markdown_indexing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(root.join("doc.md"), "# Title\nsome text").expect("write md");

        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.docs]\nenabled = false\n",
        )
        .expect("config");
        let snapshot =
            WorkspaceIndexer::build_snapshot(&cfg, std::slice::from_ref(&root), None, None);
        assert_eq!(
            snapshot.doc_index.section_count(),
            0,
            "docs engine disabled must index no markdown sections"
        );
    }

    /// `[engines.contracts] enabled = false` must stop symbol/contract extraction,
    /// instead of the flag being accepted-but-ignored.
    #[test]
    fn contracts_engine_disabled_skips_extraction() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(
            root.join("a.proto"),
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); }",
        )
        .expect("write a");

        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.contracts]\nenabled = false\n",
        )
        .expect("config");
        let snapshot =
            WorkspaceIndexer::build_snapshot(&cfg, std::slice::from_ref(&root), None, None);
        assert_eq!(
            snapshot.contract_graph.node_count(),
            0,
            "contracts engine disabled must extract no contract nodes"
        );
    }

    /// `[engines.docs] paths` must scope which markdown files get indexed instead
    /// of every `.md` file under `roots` being indexed regardless of the setting.
    #[test]
    fn docs_paths_scopes_markdown_indexing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir docs");
        std::fs::write(root.join("docs").join("adr.md"), "# ADR\nsome text").expect("write adr");
        std::fs::write(root.join("README.md"), "# Readme\nother text").expect("write readme");

        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.docs]\npaths = [\"docs/**\"]\n",
        )
        .expect("config");
        let snapshot =
            WorkspaceIndexer::build_snapshot(&cfg, std::slice::from_ref(&root), None, None);
        assert_eq!(
            snapshot.doc_index.section_count(),
            1,
            "only docs/** should be indexed when [engines.docs] paths is set"
        );
    }
}
