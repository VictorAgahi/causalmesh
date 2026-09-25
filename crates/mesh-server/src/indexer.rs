//! Workspace indexing shared by `mesh-mcp run`, `mesh-mcp graph`, `meshd` and the
//! file watcher. Previously each of those carried its own copy of the scan loop.
//!
//! Extraction (read + guard + tree-sitter) runs in parallel on the QoS-throttled
//! Rayon pool and produces graph-independent [`FileIndex`] fragments; only the
//! final fold into the `ContractGraph` and the single `reconcile_edges` pass are
//! sequential.

use mesh_core::{
    expand_roots, AppState, BackgroundRescanEngine, Config, ContractGraph, DifferentialVfs,
    DocIndex, DocSection, ExcludeMatcher, FilesystemCrawler, IndexHealth, MeshSnapshot,
    PropertyRegistry, PropertySourceMatcher, RepoId, ValidatedScope,
};
use mesh_parsers::{
    AstGuard, CompiledPattern, ExtractConfig, FileIndex, LanguageKind, PolyglotIndexer,
};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Crawl depth used for every full workspace scan.
pub const SCAN_DEPTH: usize = 10;

/// Why `process_file` never produced a fragment for a file — a deliberate,
/// counted policy rejection (see `IndexHealth`), distinct from
/// `FileIndex::parse_failed`, which means the file passed every one of these
/// and still failed inside the parser.
#[derive(Debug, Clone, Copy)]
enum RejectKind {
    /// Over the 384KB/1.5MB size budget — rejected before the file was even read.
    Oversized,
    /// Failed a lexical pre-check (binary sniff, line length, nesting depth).
    GuardRejected,
    /// Could not be read (permissions, vanished mid-scan) or was not valid UTF-8.
    ReadError,
}

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
        Self::build_snapshot_from_files(config, roots, &files, pool, vfs)
    }

    /// [`Self::build_snapshot`] over an explicit `(RepoId, path)` list instead of
    /// a fresh crawl. The determinism tests use it to feed the same files in a
    /// different order and check the snapshot fingerprint does not move.
    pub fn build_snapshot_from_files(
        config: &Config,
        roots: &[PathBuf],
        files: &[(RepoId, PathBuf)],
        pool: Option<&BackgroundRescanEngine>,
        vfs: Option<&mut DifferentialVfs>,
    ) -> MeshSnapshot {
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

        let (fragments, health) = Self::run_scan_pass(files, roots, &scan_cfg, pool);

        let mut snapshot = MeshSnapshot {
            doc_index: Self::doc_index_for(config),
            health,
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
        if !snapshot.health.is_healthy() {
            tracing::warn!(
                target: "mesh::indexer",
                "Index health: {} file(s) failed to parse, {} file(s) unreadable — see `mesh-mcp doctor`.",
                snapshot.health.files_parse_failed,
                snapshot.health.files_read_error
            );
        }
        snapshot
    }

    /// Convenience for the CLI: graph only, using the global Rayon pool.
    /// Delegates to [`Self::build_snapshot`] — one indexing path for the CLI, the
    /// server and the daemon, instead of a second one (previously duplicated here)
    /// that could silently drift out of sync with it.
    pub fn build_graph(config: &Config, roots: &[PathBuf]) -> (ContractGraph, usize) {
        let snapshot = Self::build_snapshot(config, roots, None, None);
        let file_count = snapshot.health.files_scanned;
        (snapshot.contract_graph, file_count)
    }

    /// Runs `process_file` over `files` (optionally inside `pool`), returning the
    /// resulting fragments plus this pass's `IndexHealth`. A file whose tree-sitter
    /// parse fails on the (possibly contended) parallel pass gets exactly one retry,
    /// alone on the calling thread: the wall-clock budget that used to make this
    /// outcome depend on scheduling is gone (`AstGuard::INDEX_PARSE_TIMEOUT_MICROS`
    /// is 2s), so a transient failure under load is expected to clear on a retry with
    /// nothing else competing for the CPU (idempotence invariants I1 and I6). A
    /// fragment that still has `code.parse_failed == true` after this returns is a
    /// genuine, counted failure — the caller must not fold it as if the file were
    /// simply empty.
    fn run_scan_pass(
        files: &[(RepoId, PathBuf)],
        roots: &[PathBuf],
        scan_cfg: &ScanConfig,
        pool: Option<&BackgroundRescanEngine>,
    ) -> (Vec<FileFragment>, IndexHealth) {
        let work = || {
            files
                .par_iter()
                .map(|(repo_id, path)| {
                    let root = roots
                        .get(*repo_id as usize)
                        .map_or(path.as_path(), |r| r.as_path());
                    (
                        *repo_id,
                        path.clone(),
                        Self::process_file(path, *repo_id, root, scan_cfg),
                    )
                })
                .collect::<Vec<_>>()
        };
        let results = match pool {
            Some(engine) => engine.install(work),
            None => work(),
        };

        let mut health = IndexHealth::default();
        health.record_scanned(results.len());
        let mut fragments = Vec::with_capacity(results.len());
        let mut needs_retry: Vec<(RepoId, PathBuf)> = Vec::new();
        for (repo_id, path, outcome) in results {
            match outcome {
                Ok(frag) if frag.code.parse_failed => needs_retry.push((repo_id, path)),
                Ok(frag) => {
                    health.record_indexed();
                    fragments.push(frag);
                }
                Err(reason) => Self::record_rejection(&mut health, reason),
            }
        }

        for (repo_id, path) in needs_retry {
            let root = roots
                .get(repo_id as usize)
                .map_or(path.as_path(), |r| r.as_path());
            match Self::process_file(&path, repo_id, root, scan_cfg) {
                Ok(frag) if !frag.code.parse_failed => {
                    health.record_indexed();
                    fragments.push(frag);
                }
                Ok(frag) => {
                    tracing::warn!(
                        target: "mesh::indexer",
                        "{}: tree-sitter parse failed twice (once under load, once retried alone) \
                         — indexed with no facts for this file",
                        path.display()
                    );
                    health.record_indexed();
                    health.record_parse_failed();
                    fragments.push(frag);
                }
                Err(reason) => Self::record_rejection(&mut health, reason),
            }
        }

        (fragments, health)
    }

    #[inline]
    fn record_rejection(health: &mut IndexHealth, reason: RejectKind) {
        match reason {
            RejectKind::Oversized => health.record_oversized(),
            RejectKind::GuardRejected => health.record_guard_rejected(),
            RejectKind::ReadError => health.record_read_error(),
        }
    }

    // ── Incremental reload ──────────────────────────────────────────────────

    /// Differential reload driven by the state's VFS: only files whose stat or
    /// content hash changed are re-read and re-indexed; deleted files are purged.
    /// Installs one consistent snapshot at the end.
    ///
    /// Serialized on `state.reload_lock` for its entire body, regardless of caller
    /// (`FileWatcherService::schedule_reload`, a direct test call, ...): two
    /// overlapping reloads would each `snapshot_clone()` from the *same* prior
    /// snapshot and then unconditionally `install_snapshot()` — whichever finishes
    /// last wins outright, silently discarding the other's changes rather than
    /// merging them. The lock is a plain `std::sync::Mutex`, acquired and released
    /// on this same call stack (never held across an `.await` or moved to another
    /// thread), so blocking here is exactly the intended backpressure: a second
    /// caller simply waits its turn and then reloads against then-current disk
    /// state, which already reflects whatever the first pass just did (idempotence
    /// invariant I2).
    pub fn reload(state: &AppState) {
        let _guard = state
            .reload_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let config = &state.config;
        let roots = &state.allowed_roots;
        let files = Self::crawl_all(config, roots);

        let vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());
        // Deleted: tracked by the VFS but no longer on disk. Only a full crawl's
        // complete file surface can answer this by set difference — a targeted,
        // path-driven reload (`reload_paths`) instead knows about a deletion
        // directly, from a specific watcher-reported path that no longer stat()s.
        let present: HashSet<&Path> = files.iter().map(|(_, p)| p.as_path()).collect();
        let deleted: Vec<PathBuf> = vfs
            .tracked_paths()
            .filter(|p| !present.contains(p))
            .map(Path::to_path_buf)
            .collect();
        drop(vfs);

        Self::apply_incremental(state, "VFS differential", files, deleted);
    }

    /// Targeted reload driven directly by the file watcher's own reported paths —
    /// no `crawl_all` walk of the whole tree to rediscover what might have
    /// changed. Each path is resolved to the most specific containing root
    /// (matching `crawl_all`'s own nested-root attribution) and checked against
    /// that root's exclude patterns/`.gitignore` via
    /// `FilesystemCrawler::is_path_excluded`; a path that check can't cheaply and
    /// correctly resolve (see that function's doc comment — chiefly a nested
    /// `.gitignore` between the root and the file) falls back to a full
    /// `reload()` rather than risk a wrong answer. A `.git/HEAD` or `.git/refs/*`
    /// change (checkout, rebase, branch switch) can likewise alter an arbitrary
    /// number of tracked files without each one necessarily producing its own
    /// watcher event, so that also falls back to `reload()`.
    pub fn reload_paths(state: &AppState, changed_paths: &[PathBuf]) {
        if changed_paths.is_empty() {
            return;
        }
        if changed_paths.iter().any(|p| Self::is_git_ref_change(p)) {
            tracing::debug!(
                target: "mesh::watcher",
                "Targeted reload: .git ref change in event set, falling back to full reload."
            );
            return Self::reload(state);
        }

        let config = &state.config;
        let roots = &state.allowed_roots;

        let mut seen: HashSet<&Path> = HashSet::new();
        let mut candidates: Vec<(RepoId, PathBuf)> = Vec::new();
        let mut deleted: Vec<PathBuf> = Vec::new();

        for raw in changed_paths {
            if !seen.insert(raw.as_path()) {
                continue;
            }
            let Some((repo_id, root)) = Self::most_specific_root(raw, roots) else {
                continue; // outside every allowed root — nothing to index
            };
            let exclusions =
                Self::exclude_patterns_for_root(&config.workspace.exclude_patterns, roots, root);
            let matcher = ExcludeMatcher::compile(&exclusions);
            match FilesystemCrawler::is_path_excluded(root, raw, &matcher) {
                Some(true) => continue,
                None => {
                    tracing::debug!(
                        target: "mesh::watcher",
                        "Targeted reload: could not cheaply resolve exclusion for {}, falling back to full reload.",
                        raw.display()
                    );
                    return Self::reload(state);
                }
                Some(false) => {}
            }
            if std::fs::metadata(raw).is_ok() {
                candidates.push((repo_id, raw.clone()));
            } else {
                deleted.push(raw.clone());
            }
        }

        if candidates.is_empty() && deleted.is_empty() {
            tracing::debug!(
                target: "mesh::watcher",
                "Targeted reload: no relevant candidates survived exclusion in the watcher event set."
            );
            return;
        }

        // Acquired only now, after every fallback-to-`reload()` branch above has
        // already returned — `reload()` takes this same lock itself, and
        // `std::sync::Mutex` is not reentrant.
        let _guard = state
            .reload_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self::apply_incremental(state, "Targeted", candidates, deleted);
    }

    /// The `root` in `roots` that most specifically contains `path` (the longest
    /// matching prefix) — the same attribution `crawl_all` gives an overlapping
    /// file via `exclude_patterns_for_root`'s nested-root exclusion, reproduced
    /// here for a single path without crawling anything.
    fn most_specific_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<(RepoId, &'a Path)> {
        roots
            .iter()
            .enumerate()
            .filter(|(_, root)| path.starts_with(root.as_path()))
            .max_by_key(|(_, root)| root.as_os_str().len())
            .map(|(idx, root)| (idx as RepoId, root.as_path()))
    }

    /// `.git/HEAD` or `.git/refs/...` — a branch checkout/rebase/switch can alter
    /// an arbitrary number of tracked files without each one necessarily firing
    /// its own watcher event (e.g. switching to a branch whose only difference
    /// upstream is a ref pointer). `changed_paths` is not a reliable transcript
    /// of what changed on disk in that case.
    fn is_git_ref_change(path: &Path) -> bool {
        let mut components = path.components().map(|c| c.as_os_str());
        while let Some(c) = components.next() {
            if c == ".git" {
                let next = components.next();
                return next == Some(std::ffi::OsStr::new("HEAD"))
                    || next == Some(std::ffi::OsStr::new("refs"));
            }
        }
        false
    }

    /// Shared tail of both `reload` and `reload_paths`: scan whatever candidate
    /// surface the caller resolved (a full crawl's file list, or one path-driven
    /// targeted set), diff it against the VFS, and — if anything really
    /// changed — fold it into a freshly installed snapshot. `label` only
    /// distinguishes the two callers in logs.
    fn apply_incremental(
        state: &AppState,
        label: &str,
        files: Vec<(RepoId, PathBuf)>,
        deleted: Vec<PathBuf>,
    ) {
        let config = &state.config;
        let roots = &state.allowed_roots;
        let patterns = Self::compiled_patterns(config);
        let spring = SpringSettings::from_config(config);
        let extract_cfg = Self::extract_config(config);
        let toggles = Self::engine_toggles(config);

        let mut vfs = state.vfs.lock().unwrap_or_else(|e| e.into_inner());

        // Candidates: new or stat-changed. The metadata call is parallelized over
        // Rayon threads to maximize OS kernel page-cache stat speed.
        let candidates: Vec<(RepoId, PathBuf)> = files
            .par_iter()
            .filter(|(_, p)| match std::fs::metadata(p) {
                Ok(m) => !vfs.is_unchanged_fast(p, &m),
                Err(_) => false,
            })
            .map(|(r, p)| (*r, p.clone()))
            .collect();

        if candidates.is_empty() && deleted.is_empty() {
            tracing::debug!(target: "mesh::watcher", "{label}: 0 files changed, skipping reload.");
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
        let (fragments, pass_health) =
            Self::run_scan_pass(&candidates, roots, &scan_cfg, Some(&state.rescan));

        // A file whose parse still fails after `run_scan_pass`'s retry keeps its last
        // known-good facts: it is excluded here, *before* the VFS is touched, so its
        // signature is never updated and the next reload (triggered by any change
        // anywhere) sees it as still-changed and retries it again — rather than the
        // pre-1.3 behaviour of wiping its facts to empty and marking it seen
        // (idempotence invariant I6).
        let (still_failed, fragments): (Vec<_>, Vec<_>) =
            fragments.into_iter().partition(|f| f.code.parse_failed);
        if !still_failed.is_empty() {
            tracing::warn!(
                target: "mesh::watcher",
                "{} file(s) still failed to parse after retry; keeping last known-good facts, \
                 will retry on the next reload: {:?}",
                still_failed.len(),
                still_failed
                    .iter()
                    .map(|f| f.path.display().to_string())
                    .collect::<Vec<_>>()
            );
        }

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
        snapshot.health.merge(&pass_health);
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
            "{label} reload (gen {generation}): {changed_count} re-indexed, {} removed, {node_count} contract nodes.",
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

    /// Crawls all roots, tagging each file with the `RepoId` of its **most specific**
    /// containing root — the deepest one, when roots nest (e.g. `roots = [".",
    /// "./services/*"]`, the common `[workspace] roots = [..., "./x/*"]` shape `init
    /// --auto` and hand-written configs both produce). `expand_roots` only dedupes
    /// *identical* canonical paths, so an enclosing root and its own subdirectories can
    /// both be configured roots; without this, every file under the more specific root
    /// would be indexed twice, once per root, as two nodes with different `repo_id`s
    /// that `canonical_lines()` cannot tell apart from a real duplicate declaration
    /// (idempotence invariant I4).
    ///
    /// Implemented by excluding each nested root's subtree from every one of its
    /// ancestors' crawls — one file, one crawl, one `RepoId` — rather than crawling
    /// everything and deduplicating after the fact, which would double the I/O on any
    /// workspace using this shape.
    pub fn crawl_all(config: &Config, roots: &[PathBuf]) -> Vec<(RepoId, PathBuf)> {
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
                    let exclusions = Self::exclude_patterns_for_root(
                        &config.workspace.exclude_patterns,
                        roots,
                        root,
                    );
                    let matcher = ExcludeMatcher::compile(&exclusions);
                    let files =
                        FilesystemCrawler::crawl_scope_with(&scope, &matcher, Some(SCAN_DEPTH));
                    out.extend(files.into_iter().map(|f| (repo_id, f)));
                }
                Err(e) => {
                    tracing::warn!(target: "mesh::indexer", "Skipping root {}: {e}", root.display());
                }
            }
        }
        // Deterministic order regardless of `roots`' order or the OS's directory
        // iteration order — `canonical_lines()` doesn't need it, but every other
        // consumer of this list (the differential VFS, `doctor`'s dead-config
        // reporting) benefits from a stable, reviewable file order.
        out.sort_unstable_by(|(_, a), (_, b)| a.cmp(b));
        out
    }

    /// `exclude_patterns` plus one `<relative>/**` glob per *other* configured root
    /// that is a strict descendant of `root` — so `root`'s own crawl never descends
    /// into a subtree a more specific root already owns. `roots` must already be the
    /// canonicalized, deduplicated list `expand_roots` produces (equal paths never
    /// occur twice, so "strict descendant" is the only overlap `PathBuf` prefixing can
    /// mean here).
    fn exclude_patterns_for_root(
        exclude_patterns: &[String],
        roots: &[PathBuf],
        root: &Path,
    ) -> Vec<String> {
        let mut patterns = exclude_patterns.to_vec();
        for other in roots {
            if other == root || !other.starts_with(root) {
                continue;
            }
            let Ok(rel) = other.strip_prefix(root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            patterns.push(format!("{}/**", rel.to_string_lossy().replace('\\', "/")));
        }
        patterns
    }

    /// Reads, guards and extracts one file. Runs on the pool; touches no shared state.
    ///
    /// The `Err` side is a deliberate, logged policy rejection (size/lexical guard,
    /// unreadable, not UTF-8) — every one of these is counted into `IndexHealth` by
    /// the caller, never silently dropped. A successful `Ok(frag)` can still carry
    /// `frag.code.parse_failed == true`: the file passed every guard but its
    /// tree-sitter parse itself failed (see `AstGuard::parse_with`), which the caller
    /// retries sequentially rather than folding as an empty file.
    fn process_file(
        path: &Path,
        repo_id: RepoId,
        root: &Path,
        cfg: &ScanConfig,
    ) -> Result<FileFragment, RejectKind> {
        let ScanConfig {
            patterns,
            doc_template,
            spring,
            extract_cfg,
            toggles,
        } = *cfg;
        let metadata = std::fs::metadata(path).map_err(|_| RejectKind::ReadError)?;
        // Commandment 2: check the size budget *before* reading, so an oversized file
        // never costs its full read.
        if !AstGuard::within_size_budget(path, &metadata) {
            return Err(RejectKind::Oversized);
        }
        let bytes = std::fs::read(path).map_err(|_| RejectKind::ReadError)?;
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
            return Err(RejectKind::GuardRejected);
        }
        let content = std::str::from_utf8(&bytes).map_err(|_| RejectKind::ReadError)?;
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

        Ok(frag)
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
            Ok(rel) => matcher.is_excluded_with_root(rel, Some(root)),
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

    /// The path-driven `reload_paths` must produce the exact same result as the
    /// crawl-driven `reload` for the same edit+delete, without ever calling
    /// `crawl_all` — proving the watcher's own event paths are enough.
    #[test]
    fn reload_paths_handles_edit_and_delete_without_a_full_crawl() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        let a_path = root.join("a.proto");
        let b_path = root.join("b.proto");
        std::fs::write(
            &a_path,
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); }",
        )
        .expect("write a");
        std::fs::write(
            &b_path,
            "syntax = \"proto3\"; package b; service B { rpc Y (R) returns (S); }",
        )
        .expect("write b");

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

        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(
            &a_path,
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); rpc Z (R) returns (S); }",
        )
        .expect("rewrite a");
        std::fs::remove_file(&b_path).expect("rm b");

        // Only the two paths the watcher actually reported change — a directory
        // never crawled at all is proof this didn't fall back to a full scan.
        WorkspaceIndexer::reload_paths(&state, &[a_path, b_path]);
        let view = state.snapshot();
        assert_eq!(view.generation, 2);
        assert_eq!(
            view.contract_graph.node_count(),
            3,
            "a: service + 2 rpcs; b gone"
        );
        assert!(view.contract_graph.search_symbols("B", None).is_empty());
        assert_eq!(
            state.vfs.lock().expect("vfs").len(),
            1,
            "only a.proto remains tracked"
        );
    }

    /// A brand-new file the watcher reports (a `Create` event) must be indexed by
    /// `reload_paths` even though it was never part of any prior crawl or VFS entry.
    #[test]
    fn reload_paths_indexes_a_newly_created_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        let state = make_state(&root);
        state.install_snapshot(MeshSnapshot::default());

        let c_path = root.join("c.proto");
        std::fs::write(
            &c_path,
            "syntax = \"proto3\"; package c; service C { rpc Z (R) returns (S); }",
        )
        .expect("write c");

        WorkspaceIndexer::reload_paths(&state, &[c_path]);
        let view = state.snapshot();
        assert_eq!(view.contract_graph.node_count(), 2, "service + 1 rpc");
    }

    /// `reload_paths` must not index a path that a real crawl would have pruned
    /// via `exclude_patterns` — the same exclusion `crawl_all` applies, checked
    /// here for one explicit path instead of by walking the tree.
    #[test]
    fn reload_paths_respects_exclude_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\nexclude_patterns = [\"secrets/**\"]\n",
        )
        .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let state = Arc::new(AppState::new(cfg, vec![root.clone()], audit, rescan));
        state.install_snapshot(MeshSnapshot::default());

        std::fs::create_dir_all(root.join("secrets")).expect("mkdir");
        let secret_path = root.join("secrets").join("leaked.proto");
        std::fs::write(
            &secret_path,
            "syntax = \"proto3\"; package s; service Secret { rpc X (R) returns (S); }",
        )
        .expect("write");

        WorkspaceIndexer::reload_paths(&state, &[secret_path]);
        assert_eq!(
            state.snapshot().contract_graph.node_count(),
            0,
            "an excluded path must not be indexed even when reported directly by the watcher"
        );
    }

    /// A `.git/HEAD` change (branch checkout) can alter files without each one
    /// necessarily producing its own watcher event, so `reload_paths` must fall
    /// back to a full `reload` rather than trust the reported path set alone.
    #[test]
    fn reload_paths_falls_back_to_full_reload_on_git_ref_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::write(
            root.join("a.proto"),
            "syntax = \"proto3\"; package a; service A { rpc X (R) returns (S); }",
        )
        .expect("write a");
        let state = make_state(&root);
        state.install_snapshot(MeshSnapshot::default());

        // No .proto path is in the event set at all — only a git ref path — yet
        // the real on-disk file must still be picked up via the full-reload
        // fallback, proving the fallback actually ran rather than silently
        // no-oping on an event set with nothing indexable in it.
        WorkspaceIndexer::reload_paths(&state, &[root.join(".git").join("HEAD")]);
        assert_eq!(state.snapshot().contract_graph.node_count(), 2);
    }

    /// `PropertyRegistry` provenance: deleting one of two properties files must remove
    /// exactly its keys on the next incremental reload, leaving the other file's keys intact.
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

    /// `[engines.contracts.spring]` `property_files` / `auto_redact_secrets` /
    /// `resolve_placeholders` configuration changes scan behaviour.
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

    /// `[engines.docs] paths` containing `${workspace_root}/docs/**` must be expanded
    /// properly so markdown files in `docs/` are indexed.
    #[test]
    fn docs_paths_workspace_root_expansion_scopes_indexing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir docs");
        std::fs::write(root.join("docs").join("adr.md"), "# ADR\nsome text").expect("write adr");
        std::fs::write(root.join("README.md"), "# Readme\nother text").expect("write readme");

        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n\n[engines.docs]\npaths = [\"${workspace_root}/docs/**\"]\n",
        )
        .expect("config");
        let snapshot =
            WorkspaceIndexer::build_snapshot(&cfg, std::slice::from_ref(&root), None, None);
        assert_eq!(
            snapshot.doc_index.section_count(),
            1,
            "docs/** expanded from ${{workspace_root}}/docs/** should be indexed"
        );
    }

    /// A file under a root nested inside another configured root (the common
    /// `roots = [".", "./services/*"]` shape) is indexed exactly once, attributed to
    /// the more specific (nested) root — not to both.
    #[test]
    fn crawl_all_attributes_overlapping_files_to_the_most_specific_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = dunce::canonicalize(tmp.path()).expect("canon");
        std::fs::create_dir_all(base.join("services/billing")).expect("mkdir");
        std::fs::write(base.join("services/billing/Widget.java"), "class Widget {}")
            .expect("write");
        std::fs::write(base.join("README.md"), "# root file").expect("write");

        let cfg = Config::load_from_str(
            "[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\", \"./services/*\"]\n",
        )
        .expect("config");
        let roots = vec![base.clone(), base.join("services/billing")];

        let files = WorkspaceIndexer::crawl_all(&cfg, &roots);
        let widget_hits: Vec<RepoId> = files
            .iter()
            .filter(|(_, p)| p.ends_with("Widget.java"))
            .map(|(repo_id, _)| *repo_id)
            .collect();
        assert_eq!(
            widget_hits,
            vec![1],
            "Widget.java must be crawled exactly once, attributed to the more specific \
             root (repo_id 1 = services/billing), got: {widget_hits:?}"
        );

        let readme_hits = files
            .iter()
            .filter(|(_, p)| p.ends_with("README.md"))
            .count();
        assert_eq!(
            readme_hits, 1,
            "README.md (only under the enclosing root) must still be crawled once"
        );
    }

    /// `exclude_patterns_for_root` must add a `<relative>/**` exclusion for every
    /// strict descendant root and nothing for a disjoint or non-nested one.
    #[test]
    fn exclude_patterns_for_root_only_excludes_strict_descendants() {
        let base = PathBuf::from("/ws");
        let roots = vec![
            base.clone(),
            base.join("services/billing"),
            base.join("services/checkout"),
            PathBuf::from("/other/ws"),
        ];
        let patterns =
            WorkspaceIndexer::exclude_patterns_for_root(&["**/*.pem".to_string()], &roots, &base);
        assert_eq!(
            patterns,
            vec![
                "**/*.pem".to_string(),
                "services/billing/**".to_string(),
                "services/checkout/**".to_string(),
            ]
        );

        // The most specific root has no descendants to exclude among its siblings.
        let nested = WorkspaceIndexer::exclude_patterns_for_root(&[], &roots, &roots[1]);
        assert!(nested.is_empty());
    }
}
