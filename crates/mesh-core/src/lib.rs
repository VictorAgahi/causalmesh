#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod config;
pub mod contracts;
pub mod crawler;
pub mod docs;
pub mod governance;
pub mod properties;
pub mod rescan;
pub mod security;
pub mod state;
pub mod types;
pub mod vfs;
pub mod watcher;

// Re-export common types
pub use audit::{AuditEntry, AuditError, AuditLogger};
pub use config::{
    expand_roots, AsyncApiConfig, Config, ConfigError, ContractsConfig, CustomPatternConfig,
    GrpcConfig, OpenApiConfig, PatternKind, ReadGovernanceMode, WorkspaceConfig,
};
pub use contracts::{ContractGraph, GrpcTrace, ImpactFlow};
pub use crawler::{ExcludeMatcher, FilesystemCrawler};
pub use docs::{DocIndex, DocSection};
pub use governance::{GovernanceEngine, RsahResponse};
pub use properties::{PropertyRegistry, PropertySourceMatcher};
pub use rescan::BackgroundRescanEngine;
pub use security::{SecurityError, ValidatedScope};
pub use state::{AppState, MeshSnapshot};
pub use types::{
    detect_service_package, to_pascal_case, CanonicalMethodId, CompactStr, ContractEdge,
    ContractNode, EdgeConfidence, EdgeKind, FilePath, NodeId, NodeKind, PathStr, RepoId, RepoState,
    SymbolName,
};
pub use vfs::DifferentialVfs;
pub use watcher::{FileWatcherService, ReloadFn};
