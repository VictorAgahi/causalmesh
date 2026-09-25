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

/// Confidence of a `ContractEdge`'s resolution. The graph is built from
/// exact declarations, FQCN imports, and heuristic resolutions, so an
/// edge is only as trustworthy as the strategy that produced it. This
/// lets callers weigh a result instead of treating every edge as fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgeConfidence {
    /// Resolved via a fully-qualified name (FQCN, `a.b.C`, `a::b::C`) or a
    /// structural link the graph derived directly from node identity
    /// (e.g. topic producer/consumer wiring), not from a name heuristic.
    #[default]
    Exact,
    /// Resolved via a bare symbol name, case-insensitive match, PascalCase
    /// normalization, or substring search — i.e. a heuristic that can
    /// collide across unrelated symbols that merely share a name.
    Heuristic,
    /// Multiple candidates tied and the graph could not break the tie on its
    /// own identity (same repo/package, or none of them matching the
    /// importer's) — a real homonym (e.g. two proto packages each declaring
    /// their own `AdminService`), not a resolved link. One edge is emitted per
    /// tied candidate, all tagged `Ambiguous`, rather than the graph silently
    /// picking whichever one a `HashMap` or file-processing order happened to
    /// list first (idempotence invariant I1 — that pick used to vary between
    /// otherwise-identical runs).
    Ambiguous,
}

impl EdgeConfidence {
    /// Short, human-readable label for markdown/graph surfacing.
    #[inline]
    pub fn label(&self) -> &'static str {
        match self {
            EdgeConfidence::Exact => "exact",
            EdgeConfidence::Heuristic => "heuristic",
            EdgeConfidence::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub metadata: Option<CompactStr>,
    /// How the edge's `to` target was resolved. See `EdgeConfidence`.
    #[serde(default)]
    pub confidence: EdgeConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoState {
    pub id: RepoId,
    pub name: CompactStr,
    pub root: PathBuf,
    pub file_count: usize,
}

/// Detects a canonical package or service name from optional AST package and file path.
///
/// Precedence:
/// 1. Explicit, non-generic AST package declaration (e.g. Java FQCN, Go package name, C# namespace).
///    Generic keywords like "main" fall through to directory/manifest detection.
/// 2. Deepest project manifest boundary (`Cargo.toml`, `go.mod`, `package.json`, `pom.xml`, etc.).
/// 3. Service container folder (`services/`, `apps/`, `packages/`, `modules/`, `crates/`, `libs/`, `subprojects/`).
/// 4. First non-technical parent directory (skipping `src`, `lib`, `cmd`, `pkg`, `internal`, `proto`).
/// 5. Fallback: "shared".
pub fn detect_service_package(
    file_path: &std::path::Path,
    raw_package: Option<&str>,
) -> CompactStr {
    // 1. Explicit AST package takes precedence when non-empty and non-generic
    if let Some(pkg) = raw_package {
        let trimmed = pkg.trim();
        if !trimmed.is_empty() && trimmed != "main" {
            return CompactStr::new(trimmed);
        }
    }

    // Bounds every upward directory walk below: a real project's file tree
    // is never this deep, and without a cap a file with no manifest/container
    // anywhere above it (or a relative path with a long, manifest-less
    // ancestry) walks all the way to the filesystem root — `has_compilation_manifest`
    // alone stats up to 12 candidate filenames per level, so an unbounded walk
    // multiplies that cost per file, across every language extractor, on
    // every scan. Deliberately not a persistent cross-file cache instead: a
    // manifest can appear/disappear on disk during a live daemon session, and
    // nothing here has a hook to invalidate a directory-level cache when a
    // *different* file's edit changes what one is — the depth cap only
    // bounds worst-case cost, so it can't go stale.
    const MAX_WALK_DEPTH: usize = 32;

    // 2. Walk upwards looking for compilation manifests
    let mut current = file_path.parent();
    let mut depth = 0;
    while let Some(dir) = current {
        if depth >= MAX_WALK_DEPTH {
            break;
        }
        if has_compilation_manifest(dir) {
            if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
                if !name.is_empty() {
                    return CompactStr::new(name);
                }
            }
        }
        current = dir.parent();
        depth += 1;
    }

    // 3. Check for service container convention without hardcoded microservice names
    current = file_path.parent();
    depth = 0;
    while let Some(dir) = current {
        if depth >= MAX_WALK_DEPTH {
            break;
        }
        if let Some(parent) = dir.parent() {
            if let Some(pname) = parent.file_name().and_then(|s| s.to_str()) {
                if is_container_dir(pname) {
                    if let Some(svc_name) = dir.file_name().and_then(|s| s.to_str()) {
                        return CompactStr::new(svc_name);
                    }
                }
            }
        }
        current = dir.parent();
        depth += 1;
    }

    // 4. Fallback: first non-technical directory
    current = file_path.parent();
    depth = 0;
    while let Some(dir) = current {
        if depth >= MAX_WALK_DEPTH {
            break;
        }
        if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
            if !is_technical_source_dir(name) {
                return CompactStr::new(name);
            }
        }
        current = dir.parent();
        depth += 1;
    }

    CompactStr::new("shared")
}

#[inline]
fn has_compilation_manifest(dir: &std::path::Path) -> bool {
    dir.join("go.mod").exists()
        || dir.join("Cargo.toml").exists()
        || dir.join("package.json").exists()
        || dir.join("pom.xml").exists()
        || dir.join("build.gradle").exists()
        || dir.join("build.gradle.kts").exists()
        || dir.join("pyproject.toml").exists()
        || dir.join("Pipfile").exists()
        || dir.join("setup.py").exists()
        || dir.join("Gemfile").exists()
        || dir.join("composer.json").exists()
        || dir.join("Package.swift").exists()
}

#[inline]
fn is_container_dir(name: &str) -> bool {
    matches!(
        name,
        "services"
            | "apps"
            | "packages"
            | "modules"
            | "crates"
            | "libs"
            | "subprojects"
            | "projects"
    )
}

#[inline]
fn is_technical_source_dir(name: &str) -> bool {
    matches!(
        name,
        "src" | "lib" | "cmd" | "pkg" | "internal" | "proto" | "api" | "test" | "tests"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_detect_service_package_ast_precedence() {
        let path = Path::new("services/checkout/pkg/jwt.go");
        // AST package must take precedence over directory structure
        assert_eq!(
            detect_service_package(path, Some("auth.jwt")).as_str(),
            "auth.jwt"
        );
        assert_eq!(detect_service_package(path, Some("app")).as_str(), "app");
        assert_eq!(
            detect_service_package(path, Some("custom")).as_str(),
            "custom"
        );
    }

    #[test]
    fn test_detect_service_package_main_falls_through_to_directory() {
        let path = Path::new("services/checkout/main.go");
        // "main" is generic in Go entry points, falls through to service directory
        assert_eq!(
            detect_service_package(path, Some("main")).as_str(),
            "checkout"
        );
        assert_eq!(detect_service_package(path, None).as_str(), "checkout");
    }

    #[test]
    fn test_detect_service_package_container_dirs() {
        assert_eq!(
            detect_service_package(Path::new("modules/billing/src/Payment.java"), None).as_str(),
            "billing"
        );
        assert_eq!(
            detect_service_package(Path::new("crates/mesh-core/src/lib.rs"), None).as_str(),
            "mesh-core"
        );
        assert_eq!(
            detect_service_package(Path::new("libs/auth/index.ts"), None).as_str(),
            "auth"
        );
    }

    #[test]
    fn test_detect_service_package_flat_technical_dirs() {
        assert_eq!(
            detect_service_package(Path::new("cmd/worker/main.go"), None).as_str(),
            "worker"
        );
        assert_eq!(
            detect_service_package(Path::new("pkg/storage/s3.go"), None).as_str(),
            "storage"
        );
    }

    /// Regression test for the ultrareview finding on PR #6: an unbounded
    /// upward directory walk (no manifest/container match anywhere in a very
    /// deep, all-technical-named ancestry) must terminate promptly rather
    /// than walking indefinitely — 40 nested `src/` segments exceeds the
    /// walk's depth cap, so the fallback ("shared") must still be reached
    /// without hanging or panicking.
    #[test]
    fn test_detect_service_package_terminates_on_pathologically_deep_path() {
        let deep_path: String = "src/".repeat(40) + "main.go";
        let result = detect_service_package(Path::new(&deep_path), None);
        assert_eq!(result.as_str(), "shared");
    }
}
