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
        // `Path::parent()` on a relative path eventually yields `""` (the
        // empty path) as its own final ancestor — and `"".join("Cargo.toml")`
        // resolves against the *process's own current directory*, not
        // against anything belonging to the (possibly synthetic, filesystem
        // -less) `file_path` being classified. Inside this very workspace,
        // that's always a real Cargo.toml, so an unguarded walk over a
        // relative path with no real manifest anywhere in its own ancestry
        // would silently read *this crate's own* manifest instead of finding
        // nothing. Stopping at the empty path keeps the walk scoped to
        // `file_path`'s own ancestry, exactly like `MAX_WALK_DEPTH` keeps it
        // bounded.
        if depth >= MAX_WALK_DEPTH || dir.as_os_str().is_empty() {
            break;
        }
        if has_compilation_manifest(dir) {
            // The manifest's own declared name — go.mod's `module` path's
            // last segment, package.json's `name` (npm-scope stripped),
            // Cargo.toml's `[package].name`, pyproject.toml's
            // `[project].name`/`[tool.poetry].name` — is the actual service
            // identity a human or another tool would recognize. The
            // directory name is only a fallback when the manifest has none
            // (or fails to parse): a folder named `svc` whose go.mod declares
            // `module github.com/acme/billing-service` should be identified
            // as `billing-service`, not `svc`.
            if let Some(declared) = manifest_declared_name(dir) {
                if !declared.is_empty() {
                    return CompactStr::new(declared);
                }
            }
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

/// Reads the service identity a manifest actually declares — not the
/// directory it happens to sit in, which can drift from it (a folder named
/// `svc` whose `go.mod` declares `module github.com/acme/billing-service`).
/// `None` when no manifest in `dir` declares a name, or none of them parse.
/// Callers must ensure `dir` is a real, meaningful path first (never the
/// empty path — see the caller's own guard) since this does real filesystem
/// reads.
fn manifest_declared_name(dir: &std::path::Path) -> Option<String> {
    read_go_mod_module(dir)
        .or_else(|| read_package_json_name(dir))
        .or_else(|| read_cargo_toml_name(dir))
        .or_else(|| read_pyproject_name(dir))
}

/// A real manifest's `name`/`module` declaration lives in its first few
/// lines; this is already generous for one. Unlike every other file this
/// indexing pipeline reads, a manifest lookup has no `AstGuard` size check in
/// front of it (this crate doesn't depend on `mesh-parsers`, which owns that
/// guard) — checked here directly instead, so a multi-hundred-MB `go.mod`/
/// `package.json`/`Cargo.toml`/`pyproject.toml` in a scanned repo (accidental
/// or crafted) can't force a full read into memory on every source file
/// under that directory.
const MAX_MANIFEST_SIZE_BYTES: u64 = 64 * 1024;

fn read_manifest_bounded(path: &std::path::Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_MANIFEST_SIZE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn read_go_mod_module(dir: &std::path::Path) -> Option<String> {
    let content = read_manifest_bounded(&dir.join("go.mod"))?;
    let line = content
        .lines()
        .find(|l| l.trim_start().starts_with("module "))?;
    let module_path = line.trim_start().strip_prefix("module ")?.trim();
    let name = module_path.rsplit('/').next().unwrap_or(module_path);
    (!name.is_empty()).then(|| name.to_string())
}

fn read_package_json_name(dir: &std::path::Path) -> Option<String> {
    let content = read_manifest_bounded(&dir.join("package.json"))?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let raw = json.get("name")?.as_str()?;
    // A scoped npm package name (`@scope/name`) identifies the same service
    // as its unscoped form for our purposes here.
    let name = raw.rsplit('/').next().unwrap_or(raw);
    (!name.is_empty()).then(|| name.to_string())
}

fn read_cargo_toml_name(dir: &std::path::Path) -> Option<String> {
    let content = read_manifest_bounded(&dir.join("Cargo.toml"))?;
    let value: toml::Value = content.parse().ok()?;
    value
        .get("package")?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

fn read_pyproject_name(dir: &std::path::Path) -> Option<String> {
    let content = read_manifest_bounded(&dir.join("pyproject.toml"))?;
    let value: toml::Value = content.parse().ok()?;
    value
        .get("project")
        .and_then(|p| p.get("name"))
        .or_else(|| {
            value
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|p| p.get("name"))
        })
        .and_then(|n| n.as_str())
        .map(str::to_string)
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

    /// A folder's own name and its manifest's declared name can legitimately
    /// differ (a folder named `svc` whose `go.mod` declares `module
    /// github.com/acme/billing-service`) — the declared name is the real
    /// service identity and must win. Uses a real tempdir with a real
    /// manifest file, not a synthetic path: `has_compilation_manifest`/
    /// `manifest_declared_name` do real filesystem I/O, so a fake path could
    /// only ever exercise the "no manifest found" branch.
    #[test]
    fn detect_service_package_prefers_go_mod_declared_module_over_dir_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("svc");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("go.mod"),
            "module github.com/acme/billing-service\n\ngo 1.21\n",
        )
        .unwrap();
        let file_path = svc_dir.join("main.go");

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "billing-service",
            "the go.mod-declared module name must win over the directory name 'svc'"
        );
    }

    #[test]
    fn detect_service_package_prefers_package_json_declared_name_over_dir_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("svc");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("package.json"),
            r#"{"name": "@acme/checkout-service", "version": "1.0.0"}"#,
        )
        .unwrap();
        let file_path = svc_dir.join("index.js");

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "checkout-service",
            "the npm scope must be stripped from a scoped package name"
        );
    }

    #[test]
    fn detect_service_package_prefers_cargo_toml_declared_name_over_dir_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("svc");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("Cargo.toml"),
            "[package]\nname = \"payments-core\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let file_path = svc_dir.join("src").join("lib.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "payments-core"
        );
    }

    #[test]
    fn detect_service_package_prefers_pyproject_declared_name_over_dir_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("svc");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("pyproject.toml"),
            "[tool.poetry]\nname = \"recommendation-service\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let file_path = svc_dir.join("main.py");

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "recommendation-service"
        );
    }

    /// A manifest with no name field the reader understands (or one that
    /// fails to parse) must fall back to the directory name exactly as
    /// before this feature — not to an empty string or a panic.
    #[test]
    fn detect_service_package_falls_back_to_dir_name_when_manifest_has_no_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("checkout");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(svc_dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        let file_path = svc_dir.join("main.rs");

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "checkout"
        );
    }

    /// A manifest larger than `MAX_MANIFEST_SIZE_BYTES` must not be read into
    /// memory at all — falls back to the directory name, the same as an
    /// unparseable one, instead of a full read on every source file under a
    /// directory whose manifest happens to be huge (accidental or crafted).
    #[test]
    fn detect_service_package_does_not_read_an_oversized_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc_dir = dir.path().join("checkout");
        std::fs::create_dir_all(&svc_dir).unwrap();
        let oversized = format!(
            "[package]\nname = \"should-be-ignored\"\n# {}\n",
            "x".repeat(70 * 1024)
        );
        std::fs::write(svc_dir.join("Cargo.toml"), oversized).unwrap();
        let file_path = svc_dir.join("src").join("main.rs");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        assert_eq!(
            detect_service_package(&file_path, None).as_str(),
            "checkout",
            "an oversized manifest must be skipped entirely, not read and parsed"
        );
    }

    /// Regression for the empty-path pitfall `manifest_declared_name`
    /// introduced a real risk for: a synthetic relative path with no real
    /// manifest anywhere in its own ancestry must never read *this crate's
    /// own* real `Cargo.toml` (name `"mesh-core"`) just because
    /// `Path::parent()` eventually yields the empty path, which resolves
    /// against the process's actual cwd.
    #[test]
    fn detect_service_package_does_not_leak_the_running_crates_own_manifest() {
        let result = detect_service_package(Path::new("totally/synthetic/path/main.go"), None);
        assert_ne!(
            result.as_str(),
            "mesh-core",
            "must not silently resolve to this crate's own real Cargo.toml via the empty-path ancestor"
        );
    }
}
