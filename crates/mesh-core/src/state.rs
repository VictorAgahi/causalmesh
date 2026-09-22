use crate::audit::AuditLogger;
use crate::config::Config;
use crate::contracts::ContractGraph;
use crate::docs::DocIndex;
use crate::governance::GovernanceEngine;
use crate::properties::PropertyRegistry;
use crate::rescan::BackgroundRescanEngine;
use crate::vfs::DifferentialVfs;
use arc_swap::{ArcSwap, Guard};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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
    /// into one rescan instead of piling identical jobs on the Rayon pool.
    pub reload_pending: AtomicBool,
}

impl AppState {
    pub fn new(
        config: Config,
        allowed_roots: Vec<PathBuf>,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
    ) -> Self {
        let (stop_rules, skills) = config
            .engines
            .policy
            .as_ref()
            .map(|p| (p.stop_rules.clone(), p.skills.clone()))
            .unwrap_or_default();

        Self {
            config: Arc::new(config),
            allowed_roots: allowed_roots.into(),
            governance: Arc::new(GovernanceEngine::new(stop_rules, skills)),
            snapshot: ArcSwap::from_pointee(MeshSnapshot::default()),
            audit,
            rescan,
            vfs: Mutex::new(DifferentialVfs::new()),
            reload_pending: AtomicBool::new(false),
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
}
