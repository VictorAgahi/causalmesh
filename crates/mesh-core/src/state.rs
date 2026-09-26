use crate::audit::AuditLogger;
use crate::config::Config;
use crate::contracts::ContractGraph;
use crate::docs::DocIndex;
use crate::governance::GovernanceEngine;
use crate::health::IndexHealth;
use crate::index_cache::PersistentIndexCache;
use crate::properties::PropertyRegistry;
use crate::rescan::BackgroundRescanEngine;
use crate::search_cache::SearchCache;
use crate::vfs::DifferentialVfs;
use arc_swap::{ArcSwap, Guard};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};

/// One immutable, internally consistent view of the indexed workspace.
///
/// The three indices are always produced by the same indexing pass and swapped
/// in together, so a reader can never observe a new `contract_graph` next to a
/// stale `doc_index` (which separate `ArcSwap`s per field allowed).
#[derive(Debug, Clone, Default)]
pub struct MeshSnapshot {
    pub contract_graph: ContractGraph,
    pub doc_index: DocIndex,
    pub property_registry: PropertyRegistry,
    /// Monotonic counter bumped on every install; lets callers detect a reload.
    pub generation: u64,
    /// Counts of what happened to every file since the last full rebuild.
    /// Diagnostic, not content: excluded from `fingerprint()` for the same
    /// reason `generation` is — it describes this build's run, not what it
    /// found — but a tool footer or `mesh-mcp doctor` should surface it
    /// whenever `!health.is_healthy()`, so a file that didn't make it into the
    /// graph is never silently indistinguishable from a legitimately empty one.
    pub health: IndexHealth,
}

/// Content fingerprint of a [`MeshSnapshot`]: one SHA-256 per index plus a
/// combined hash. Two snapshots built from the same workspace state must have
/// equal fingerprints (idempotence invariants I1–I3); `generation` is a reload
/// counter, not content, and is deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFingerprint {
    pub combined: String,
    pub graph: String,
    pub docs: String,
    pub properties: String,
    pub nodes: usize,
    pub edges: usize,
    pub doc_sections: usize,
    pub property_keys: usize,
}

impl std::fmt::Display for SnapshotFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "fingerprint: {}", self.combined)?;
        writeln!(
            f,
            "graph:       {} ({} nodes, {} edges)",
            self.graph, self.nodes, self.edges
        )?;
        writeln!(
            f,
            "docs:        {} ({} sections)",
            self.docs, self.doc_sections
        )?;
        write!(
            f,
            "properties:  {} ({} keys)",
            self.properties, self.property_keys
        )
    }
}

impl MeshSnapshot {
    /// Fingerprints the three indices from their canonical forms.
    pub fn fingerprint(&self) -> SnapshotFingerprint {
        let sha = |lines: &[String]| AuditLogger::compute_sha256(lines.join("\n").as_bytes());
        let graph = sha(&self.contract_graph.canonical_lines());
        let docs = sha(&self.doc_index.canonical_lines());
        let properties = sha(&self.property_registry.canonical_lines());
        let combined =
            AuditLogger::compute_sha256(format!("{graph}\n{docs}\n{properties}").as_bytes());
        SnapshotFingerprint {
            combined,
            graph,
            docs,
            properties,
            nodes: self.contract_graph.node_count(),
            edges: self.contract_graph.edge_count(),
            doc_sections: self.doc_index.section_count(),
            property_keys: self.property_registry.len(),
        }
    }
}

/// Central application state per RFC-001 Commandment 1.
///
/// Only `snapshot` is hot-swapped (lock-free, copy-on-write). Everything else is
/// fixed for the lifetime of the process and lives behind a plain `Arc`.
pub struct AppState {
    pub config: Arc<Config>,
    pub allowed_roots: Arc<[PathBuf]>,
    pub governance: Arc<GovernanceEngine>,
    pub snapshot: ArcSwap<MeshSnapshot>,
    pub audit: Arc<AuditLogger>,
    pub rescan: Arc<BackgroundRescanEngine>,
    /// Content-hash cache used by incremental reloads. Per-state, not process-global,
    /// so two `AppState`s in one process (tests) don't share signatures.
    pub vfs: Mutex<DifferentialVfs>,
    /// Set while a reload is queued or running; coalesces bursts of watcher events
    /// into one rescan instead of piling identical jobs on the Rayon pool. Purely
    /// an optimization — correctness (never two `WorkspaceIndexer::reload` calls
    /// running at once) comes from `reload_lock` below, not from this flag.
    pub reload_pending: AtomicBool,
    /// Watcher-reported paths accumulated for the next reload job to drain — see
    /// `FileWatcherService::schedule_reload`. A burst that arrives while
    /// `reload_pending` is already set still appends here instead of being
    /// dropped, so a path-driven targeted reload (`WorkspaceIndexer::reload_paths`)
    /// never silently misses a change just because it coalesced with another.
    pub pending_reload_paths: Mutex<Vec<PathBuf>>,
    /// Held for the full duration of one `WorkspaceIndexer::reload` call, entirely
    /// on the single Rayon-pool thread that acquired it (a `std::sync::MutexGuard`
    /// never crosses threads here). Two reload closures can still both get spawned
    /// (`reload_pending`'s coalescing check is best-effort, not exclusive), but the
    /// second one simply blocks here until the first finishes and then runs its own
    /// pass against then-current disk state — at most one reload ever mutates the
    /// snapshot at a time, so a slower first pass can never install a snapshot that
    /// clobbers a second, newer one that finished first (idempotence invariant I2).
    pub reload_lock: Mutex<()>,
    /// `smart_search` result pages, invalidated on every snapshot generation bump.
    pub search_cache: SearchCache,
    /// This workspace's persistent index cache (plan 4 step 4.4), opened once at boot by
    /// [`AppState::open_index_cache`] and shared by the boot scan and every later reload, so
    /// reloads both hit it (a branch switch back to already-seen content) and keep it under
    /// its quota. Empty when opening failed or in tests: indexing then parses everything.
    pub index_cache: OnceLock<PersistentIndexCache>,
}

impl AppState {
    pub fn new(
        config: Config,
        allowed_roots: Vec<PathBuf>,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
    ) -> Self {
        // `[engines.policy] enabled = false` switches the whole policy engine off:
        // no stop rules, no skill recommendations.
        let (stop_rules, skills, read_governance_mode) = config
            .engines
            .policy
            .as_ref()
            .filter(|p| p.enabled)
            .map(|p| {
                (
                    p.stop_rules.clone(),
                    p.skills.clone(),
                    p.read_governance_mode,
                )
            })
            .unwrap_or_default();

        Self {
            config: Arc::new(config),
            allowed_roots: allowed_roots.into(),
            governance: Arc::new(GovernanceEngine::new(
                stop_rules,
                skills,
                read_governance_mode,
            )),
            snapshot: ArcSwap::from_pointee(MeshSnapshot::default()),
            audit,
            rescan,
            vfs: Mutex::new(DifferentialVfs::new()),
            reload_pending: AtomicBool::new(false),
            pending_reload_paths: Mutex::new(Vec::new()),
            reload_lock: Mutex::new(()),
            search_cache: SearchCache::default(),
            index_cache: OnceLock::new(),
        }
    }

    /// Opens the persistent index cache of the workspace rooted at `base_dir`
    /// (`~/.cache/mesh-mcp/workspaces/<workspace_id>/index-cache.db`, quota from
    /// `[cache] max_size_mb`) and attaches it to this state. A cache that cannot be opened
    /// (permissions, disk full) degrades to "parse everything" with a warning instead of
    /// failing the boot: it is a performance optimization, never a correctness dependency.
    /// Idempotent: a second call returns the cache already attached.
    pub fn open_index_cache(&self, base_dir: &Path) -> Option<&PersistentIndexCache> {
        if let Some(cache) = self.index_cache.get() {
            return Some(cache);
        }
        match PersistentIndexCache::open_for_workspace(base_dir, self.config.cache.max_size_mb) {
            Ok(cache) => Some(self.index_cache.get_or_init(|| cache)),
            Err(e) => {
                tracing::warn!(
                    target: "mesh::index_cache",
                    "Failed to open persistent index cache, continuing without it: {e}"
                );
                None
            }
        }
    }

    /// Lock-free read of the current snapshot. Hold the guard only for the duration
    /// of one request; it pins the snapshot alive but never blocks a writer.
    #[inline]
    pub fn snapshot(&self) -> Guard<Arc<MeshSnapshot>> {
        self.snapshot.load()
    }

    /// Atomically publishes a new snapshot, stamping it with the next generation.
    pub fn install_snapshot(&self, mut snapshot: MeshSnapshot) -> u64 {
        let generation = self.snapshot.load().generation + 1;
        snapshot.generation = generation;
        self.snapshot.store(Arc::new(snapshot));
        generation
    }

    /// Clones the current snapshot so it can be mutated and re-installed.
    #[inline]
    pub fn snapshot_clone(&self) -> MeshSnapshot {
        (**self.snapshot.load()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContractNode, NodeKind};

    fn state() -> AppState {
        let config =
            Config::load_from_str("[workspace]\nname = \"t\"\nversion = \"0\"\nroots = [\".\"]\n")
                .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        AppState::new(config, vec![], audit, rescan)
    }

    #[test]
    fn policy_enabled_false_disables_stop_rules_and_skills() {
        let config = Config::load_from_str(
            r#"
[workspace]
name = "t"
version = "0"
roots = ["."]

[engines.policy]
enabled = false

[engines.policy.stop_rules]
"proto-registry" = "STOP"

[engines.policy.skills]
"proto-registry" = "skills/proto.md"
"#,
        )
        .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let st = AppState::new(config, vec![], audit, rescan);

        // With the engine disabled, neither the stop rule nor the skill hint fires,
        // even though both were configured.
        assert!(st.governance.evaluate_guard("proto-registry").is_none());
        assert!(st
            .governance
            .recommend_skill("smart_search", Some("proto-registry"))
            .is_none());
    }

    #[test]
    fn policy_enabled_true_keeps_stop_rules_and_skills() {
        let config = Config::load_from_str(
            r#"
[workspace]
name = "t"
version = "0"
roots = ["."]

[engines.policy.stop_rules]
"proto-registry" = "STOP"

[engines.policy.skills]
"proto-registry" = "skills/proto.md"
"#,
        )
        .expect("config");
        let audit = Arc::new(AuditLogger::new_in_memory().expect("audit"));
        let rescan = Arc::new(BackgroundRescanEngine::new().expect("rescan"));
        let st = AppState::new(config, vec![], audit, rescan);

        assert!(st.governance.evaluate_guard("proto-registry").is_some());
        assert!(st
            .governance
            .recommend_skill("smart_search", Some("proto-registry"))
            .is_some());
    }

    #[test]
    fn install_snapshot_is_atomic_and_monotonic() {
        let st = state();
        assert_eq!(st.snapshot().generation, 0);

        let mut snap = st.snapshot_clone();
        snap.contract_graph.add_node(ContractNode {
            id: 0,
            name: "A".into(),
            kind: NodeKind::ServiceClass,
            file_path: std::path::Path::new("a.rs").into(),
            line_start: 1,
            line_end: 1,
            package: "p".into(),
            repo_id: 0,
            signature: None,
            docstring: None,
        });
        snap.doc_index
            .index_markdown_file(std::path::Path::new("d.md"), "# T\nbody");

        let gen = st.install_snapshot(snap);
        assert_eq!(gen, 1);

        // One guard, one consistent view: both indices come from the same install.
        let view = st.snapshot();
        assert_eq!(view.generation, 1);
        assert_eq!(view.contract_graph.node_count(), 1);
        assert_eq!(view.doc_index.section_count(), 1);
    }

    #[test]
    fn fingerprint_ignores_generation_but_tracks_every_index() {
        let st = state();
        let mut snap = st.snapshot_clone();
        let before = snap.fingerprint();

        snap.generation = 42;
        assert_eq!(
            snap.fingerprint(),
            before,
            "generation is a reload counter, not content"
        );

        snap.doc_index
            .index_markdown_file(std::path::Path::new("d.md"), "# T\nbody");
        let with_doc = snap.fingerprint();
        assert_ne!(with_doc.docs, before.docs);
        assert_eq!(with_doc.graph, before.graph);
        assert_eq!(with_doc.properties, before.properties);
        assert_ne!(with_doc.combined, before.combined);
    }
}
