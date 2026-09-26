#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod config;
pub mod contracts;
pub mod crawler;
pub mod docs;
pub mod governance;
pub mod health;
pub mod index_cache;
pub mod paths;
pub mod properties;
pub mod rescan;
pub mod search_cache;
pub mod security;
pub mod socket;
pub mod state;
pub mod types;
pub mod vfs;
pub mod watcher;

// Re-export common types
pub use audit::{AuditEntry, AuditError, AuditLogger};
pub use config::{
    expand_roots, strip_workspace_root_prefix, AsyncApiConfig, Config, ConfigError,
    ContractsConfig, CustomPatternConfig, GrpcConfig, OpenApiConfig, PatternKind,
    ReadGovernanceMode, WorkspaceConfig,
};
pub use contracts::{ContractGraph, GrpcTrace, ImpactFlow};
pub use crawler::{ExcludeMatcher, FilesystemCrawler, WatchPlan};
pub use docs::{DocIndex, DocSection};
pub use governance::{GovernanceEngine, RsahResponse};
pub use health::IndexHealth;
pub use index_cache::{sha256, CacheEntry, IndexCacheError, PersistentIndexCache};
pub use paths::mesh_cache_dir;
pub use properties::{PropertyRegistry, PropertySourceMatcher};
pub use rescan::BackgroundRescanEngine;
pub use search_cache::{file_stamp, CachedSearch, FileStamp, SearchCache, SearchCacheKey};
pub use security::{SecurityError, ValidatedScope};
pub use socket::{cleanup_stale_socket, socket_path, socket_path_for, workspace_id};
#[cfg(windows)]
pub use socket::{pipe_name, pipe_name_for};
pub use state::{AppState, MeshSnapshot, SnapshotFingerprint};
pub use types::{
    detect_service_package, to_pascal_case, CanonicalMethodId, CompactStr, ContractEdge,
    ContractNode, EdgeConfidence, EdgeKind, FilePath, NodeId, NodeKind, PathStr, RepoId, RepoState,
    SymbolName,
};
pub use vfs::DifferentialVfs;
pub use watcher::{FileWatcherService, ReloadFn};
