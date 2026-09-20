use crate::audit::AuditLogger;
use crate::config::Config;
use crate::contracts::ContractGraph;
use crate::docs::DocIndex;
use crate::governance::GovernanceEngine;
use crate::properties::PropertyRegistry;
use crate::rescan::BackgroundRescanEngine;
use crate::types::{CompactStr, NodeId, RepoId, RepoState};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Central lock-free application state utilizing ArcSwap for map-level Copy-on-Write.
pub struct AppState {
    pub config: ArcSwap<Config>,
    pub doc_index: ArcSwap<DocIndex>,
    pub contract_graph: ArcSwap<ContractGraph>,
    pub property_registry: ArcSwap<PropertyRegistry>,
    pub repo_states: ArcSwap<HashMap<RepoId, Arc<RepoState>>>,
    pub repo_lookup: ArcSwap<HashMap<CompactStr, RepoId>>,
    pub file_to_nodes: ArcSwap<HashMap<PathBuf, Vec<NodeId>>>,
    pub allowed_roots: ArcSwap<Vec<PathBuf>>,
    pub governance: ArcSwap<GovernanceEngine>,
    pub audit: Arc<AuditLogger>,
    pub rescan: Arc<BackgroundRescanEngine>,
}

impl AppState {
    pub fn new(
        config: Config,
        allowed_roots: Vec<PathBuf>,
        audit: Arc<AuditLogger>,
        rescan: Arc<BackgroundRescanEngine>,
    ) -> Self {
        let stop_rules = config
            .engines
            .policy
            .as_ref()
            .map(|p| p.stop_rules.clone())
            .unwrap_or_default();
        let skills = config
            .engines
            .policy
            .as_ref()
            .map(|p| p.skills.clone())
            .unwrap_or_default();

        let governance = GovernanceEngine::new(stop_rules, skills);
        let doc_index = DocIndex::default();
        let contract_graph = ContractGraph::default();
        let property_registry = PropertyRegistry::default();

        Self {
            config: ArcSwap::from_pointee(config),
            doc_index: ArcSwap::from_pointee(doc_index),
            contract_graph: ArcSwap::from_pointee(contract_graph),
            property_registry: ArcSwap::from_pointee(property_registry),
            repo_states: ArcSwap::from_pointee(HashMap::new()),
            repo_lookup: ArcSwap::from_pointee(HashMap::new()),
            file_to_nodes: ArcSwap::from_pointee(HashMap::new()),
            allowed_roots: ArcSwap::from_pointee(allowed_roots),
            governance: ArcSwap::from_pointee(governance),
            audit,
            rescan,
        }
    }
}
