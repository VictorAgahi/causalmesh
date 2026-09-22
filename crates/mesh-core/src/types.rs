use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Re-export CompactString as CompactStr for zero-allocation hot loops per RFC-001 Commandment 1
pub type CompactStr = compact_str::CompactString;

pub type RepoId = u16;

pub type SymbolName = CompactStr;
pub type PathStr = CompactStr;
pub type NodeId = u32;
pub type EdgeId = u32;

/// Interned file path shared by every node declared in the same file
/// (Commandment 1: one heap buffer per file, not one per symbol).
pub type FilePath = Arc<Path>;

/// Canonical FQCN projection: package.ServiceName/MethodName
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalMethodId(CompactStr);

impl CanonicalMethodId {
    pub fn new(package: &str, service: &str, method: &str) -> Self {
        let norm_service = to_pascal_case(service);
        let norm_method = to_pascal_case(method);
        Self(CompactStr::new(format!(
            "{package}.{norm_service}/{norm_method}"
        )))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[inline]
    pub fn into_compact_str(self) -> CompactStr {
        self.0
    }
}

impl std::fmt::Display for CanonicalMethodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Helper to normalize strings to PascalCase for cross-language FQCN matching
pub fn to_pascal_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;

    for c in s.chars() {
        if c == '_' || c == '-' || c == '.' || c == '/' || c == ' ' {
            capitalize_next = true;
        } else if capitalize_next {
            result.extend(c.to_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }

    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    GrpcService,
    GrpcMethod,
    HttpEndpoint,
    KafkaTopic,
    EventStream,
    Queue,
    ProtoMessage,
    PostProcessor,
    Saga,
    ServiceClass,
    Interface,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractNode {
    pub id: NodeId,
    pub name: SymbolName,
    pub kind: NodeKind,
    pub file_path: FilePath,
    pub line_start: usize,
    pub line_end: usize,
    pub package: CompactStr,
    pub repo_id: RepoId,
    pub signature: Option<CompactStr>,
    pub docstring: Option<CompactStr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Produces,
    Consumes,
    CallsRpc,
    Implements,
    Imports,
    DispatchesTo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub metadata: Option<CompactStr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub id: RepoId,
    pub name: CompactStr,
    pub root: PathBuf,
    pub file_count: usize,
}

/// Heuristic to detect a clean service or package name from file path and optional AST package
pub fn detect_service_package(
    file_path: &std::path::Path,
    raw_package: Option<&str>,
) -> CompactStr {
    // 1. If inside a services/, apps/, or packages/ directory, microservice folder is canonical!
    let mut current = file_path.parent();
    while let Some(dir) = current {
        if let Some(parent) = dir.parent() {
            if let Some(pname) = parent.file_name().and_then(|s| s.to_str()) {
                if pname == "services" || pname == "apps" || pname == "packages" {
                    if let Some(svc_name) = dir.file_name().and_then(|s| s.to_str()) {
                        return CompactStr::new(svc_name);
                    }
                }
            }
        }
        current = dir.parent();
    }

    // 2. Otherwise use explicit non-generic raw package (e.g. proto package shop.checkout.v1)
    if let Some(pkg) = raw_package {
        let trimmed = pkg.trim();
        if !trimmed.is_empty()
            && trimmed != "main"
            && trimmed != "app"
            && trimmed != "crate"
            && trimmed != "src"
            && trimmed != "module"
            && trimmed != "custom"
        {
            return CompactStr::new(trimmed);
        }
    }

    // 3. Fallback directory inspection
    current = file_path.parent();
    while let Some(dir) = current {
        if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
            if name != "src"
                && name != "lib"
                && name != "cmd"
                && name != "pkg"
                && name != "internal"
                && name != "services"
                && name != "proto"
            {
                return CompactStr::new(name);
            }
        }
        current = dir.parent();
    }

    CompactStr::new("shared")
}
