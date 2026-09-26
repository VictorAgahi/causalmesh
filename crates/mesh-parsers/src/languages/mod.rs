pub mod cpp;
pub mod csharp;
pub mod go;
pub mod java;
pub mod kotlin;
pub mod php;
pub mod proto;
pub mod python;
pub mod ruby;
pub mod rust_lang;
pub mod scala;
mod spec_shape;
pub mod swift;
pub mod ts_config;
pub mod typescript;

pub use ts_config::TsConfigResolver;

use crate::decapitate::LanguageKind;
use crate::guard::{AstGuard, ParseOutcome};
use mesh_core::{
    CompactStr, ContractGraph, ContractNode, ContractsConfig, CustomPatternConfig, NodeKind,
    PatternKind, RepoId,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Default TS decorator that marks a method as a gRPC handler when
/// `[engines.contracts.grpc] controller_annotations` is not configured.
const DEFAULT_CONTROLLER_ANNOTATION: &str = "@GrpcMethod";

/// Resolved extraction-time knobs pulled from `[engines.contracts.*]`. Bundled
/// so `PolyglotIndexer` call sites only thread one value through the pool
/// instead of four independent config lookups per file.
#[derive(Debug, Clone)]
pub struct ExtractConfig {
    /// Directories (matched as a path substring) that scope where `.proto`
    /// files are extracted from. Empty means unrestricted (legacy behaviour:
    /// every `.proto` file, wherever it lives).
    pub proto_dirs: Vec<String>,
    /// TS decorators that mark a method as a gRPC handler. Empty falls back
    /// to the historical hardcoded `@GrpcMethod`.
    pub controller_annotations: Vec<String>,
    /// When true (default), a proto RPC node is named `Service.Method`
    /// (canonical). When false, it is projected as the bare method name.
    pub canonical_fqcn_projection: bool,
    /// Path patterns (substring/suffix match) that scope which files are
    /// treated as OpenAPI specs. Empty falls back to filename/content sniffing.
    pub openapi_spec_files: Vec<String>,
    /// Same as `openapi_spec_files`, for AsyncAPI.
    pub asyncapi_spec_files: Vec<String>,
    /// When true (default), AsyncAPI extraction also infers topics from a
    /// non-standard top-level `topics:` string list, not just `channels`.
    pub infer_string_topics: bool,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            proto_dirs: Vec::new(),
            controller_annotations: vec![DEFAULT_CONTROLLER_ANNOTATION.to_string()],
            canonical_fqcn_projection: true,
            openapi_spec_files: Vec::new(),
            asyncapi_spec_files: Vec::new(),
            infer_string_topics: true,
        }
    }
}

impl ExtractConfig {
    /// Builds the extraction config from `[engines.contracts]`. Missing
    /// sub-tables (`grpc`/`openapi`/`asyncapi`) fall back to their defaults.
    pub fn from_contracts(contracts: &ContractsConfig) -> Self {
        let default = Self::default();
        let (proto_dirs, controller_annotations, canonical_fqcn_projection) = match &contracts.grpc
        {
            Some(g) => {
                let annotations = if g.controller_annotations.is_empty() {
                    default.controller_annotations.clone()
                } else {
                    g.controller_annotations.clone()
                };
                (
                    g.proto_dirs.clone(),
                    annotations,
                    g.canonical_fqcn_projection,
                )
            }
            None => (
                default.proto_dirs.clone(),
                default.controller_annotations.clone(),
                default.canonical_fqcn_projection,
            ),
        };
        let openapi_spec_files = contracts
            .openapi
            .as_ref()
            .map(|o| o.spec_files.clone())
            .unwrap_or_default();
        let (asyncapi_spec_files, infer_string_topics) = match &contracts.asyncapi {
            Some(a) => (a.spec_files.clone(), a.infer_string_topics),
            None => (Vec::new(), default.infer_string_topics),
        };

        Self {
            proto_dirs,
            controller_annotations,
            canonical_fqcn_projection,
            openapi_spec_files,
            asyncapi_spec_files,
            infer_string_topics,
        }
    }

    /// Whether `path` falls under one of `proto_dirs` (or `proto_dirs` is
    /// empty, i.e. unrestricted).
    pub fn allows_proto_path(&self, path_str: &str) -> bool {
        if self.proto_dirs.is_empty() {
            return true;
        }
        let normalized = path_str.replace('\\', "/");
        self.proto_dirs.iter().any(|d| {
            let clean = mesh_core::strip_workspace_root_prefix(d);
            let needle = clean.trim_matches('/').replace('\\', "/");
            !needle.is_empty() && normalized.contains(needle.as_str())
        })
    }

    /// Whether `path` matches one of `openapi_spec_files`.
    pub fn allows_openapi_spec(&self, path_str: &str) -> bool {
        if self.openapi_spec_files.is_empty() {
            return false;
        }
        let lower = path_str.to_lowercase().replace('\\', "/");
        self.openapi_spec_files
            .iter()
            .any(|spec| spec_file_matches(&lower, spec))
    }
}

/// Matches `path_str` (already lowercased) against a configured spec-file
/// pattern by suffix or substring, mirroring `CompiledPattern::matches_path`.
fn spec_file_matches(path_str: &str, pattern: &str) -> bool {
    let clean = mesh_core::strip_workspace_root_prefix(pattern);
    let needle = clean
        .trim_start_matches("./")
        .trim_start_matches('*')
        .to_lowercase();
    !needle.is_empty()
        && (path_str.ends_with(needle.as_str()) || path_str.contains(needle.as_str()))
}

/// Everything extracted from one file, expressed against *local* node indices.
///
/// Extraction is decoupled from graph mutation so it can run on the Rayon pool
/// (`ContractGraph` is `&mut` and single-writer). `apply` assigns real `NodeId`s
/// by inserting into the graph sequentially.
///
/// `Serialize`/`Deserialize`: this is exactly the unit `PersistentIndexCache` (P2 step 3.2)
/// persists per content hash — pre-`NodeId`-numbering, so a cache hit skips tree-sitter
/// entirely without touching global node numbering (see `mesh_core::index_cache`'s module
/// doc).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub nodes: Vec<ContractNode>,
    /// (local node index, imported target)
    pub dependencies: Vec<(usize, CompactStr)>,
    /// (local node index, topic)
    pub producers: Vec<(usize, CompactStr)>,
    /// (local node index, topic)
    pub consumers: Vec<(usize, CompactStr)>,
    /// (local node index, target rpc)
    pub rpc_calls: Vec<(usize, CompactStr)>,
    /// Set when this file's tree-sitter parse itself failed or exceeded its budget
    /// (`AstGuard::ParseOutcome::ParseFailed`) — as opposed to `nodes` legitimately
    /// being empty (a blank file, one with no top-level declarations, ...). Callers
    /// must not treat this file as "indexed with zero facts": `WorkspaceIndexer` gives
    /// it one sequential retry outside the contended parallel pool on a full build,
    /// and on an incremental reload keeps its last known-good facts and retries on the
    /// next change, rather than wiping them (idempotence invariant I6).
    pub parse_failed: bool,
}

impl FileIndex {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Appends `other` after `self`, shifting its local indices.
    pub fn merge(&mut self, other: FileIndex) {
        let offset = self.nodes.len();
        self.nodes.extend(other.nodes);
        self.dependencies
            .extend(other.dependencies.into_iter().map(|(i, t)| (i + offset, t)));
        self.producers
            .extend(other.producers.into_iter().map(|(i, t)| (i + offset, t)));
        self.consumers
            .extend(other.consumers.into_iter().map(|(i, t)| (i + offset, t)));
        self.rpc_calls
            .extend(other.rpc_calls.into_iter().map(|(i, t)| (i + offset, t)));
        self.parse_failed |= other.parse_failed;
    }

    /// Inserts the extracted nodes and relations into `graph`.
    pub fn apply(self, graph: &mut ContractGraph) {
        let ids: Vec<_> = self.nodes.into_iter().map(|n| graph.add_node(n)).collect();
        for (i, target) in self.dependencies {
            graph.add_dependency(ids[i], &target);
        }
        for (i, topic) in self.producers {
            graph.add_producer(ids[i], &topic);
        }
        for (i, topic) in self.consumers {
            graph.add_consumer(ids[i], &topic);
        }
        for (i, target) in self.rpc_calls {
            graph.add_rpc_call(ids[i], &target);
        }
    }
}

/// A `CustomPatternConfig` with its regex compiled once, instead of once per file.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub kind: PatternKind,
    /// Normalised `file_pattern` (leading `*`/`.` stripped), matched by suffix or substring.
    file_needle: Option<String>,
    regex: regex::Regex,
    target_group: usize,
    consumer_group: Option<usize>,
}

impl CompiledPattern {
    pub fn compile(cfg: &CustomPatternConfig) -> Result<Self, regex::Error> {
        Ok(Self {
            kind: cfg.kind,
            file_needle: cfg.file_pattern.as_deref().map(|fp| {
                fp.trim_start_matches('*')
                    .trim_start_matches('.')
                    .to_string()
            }),
            regex: regex::Regex::new(&cfg.regex)?,
            target_group: cfg.target_group,
            consumer_group: cfg.consumer_group,
        })
    }

    /// Compiles every pattern, logging and skipping invalid ones rather than
    /// silently ignoring them on every file like the per-file compile did.
    pub fn compile_all(cfgs: &[CustomPatternConfig]) -> Vec<Self> {
        cfgs.iter()
            .filter_map(|cfg| match Self::compile(cfg) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(
                        target: "mesh::indexer",
                        "Skipping custom pattern '{}': invalid regex: {e}",
                        cfg.name
                    );
                    None
                }
            })
            .collect()
    }

    #[inline]
    fn matches_path(&self, path_str: &str) -> bool {
        match &self.file_needle {
            Some(needle) => {
                path_str.ends_with(needle.as_str()) || path_str.contains(needle.as_str())
            }
            None => true,
        }
    }
}

pub struct PolyglotIndexer;

impl PolyglotIndexer {
    /// Ingests and indexes a source file into the ContractGraph.
    pub fn index_file(file_path: &Path, content: &str, repo_id: RepoId, graph: &mut ContractGraph) {
        Self::extract(file_path, content, repo_id).apply(graph);
    }

    /// Parses a source file into a graph-independent `FileIndex`. Safe to call from any thread.
    /// Uses the default `ExtractConfig` — legacy behaviour for callers that don't
    /// thread `[engines.contracts]` through (tests, ad-hoc CLI usage).
    pub fn extract(file_path: &Path, content: &str, repo_id: RepoId) -> FileIndex {
        Self::extract_with_config(file_path, content, repo_id, &ExtractConfig::default())
    }

    /// Same as [`Self::extract`], but honours `[engines.contracts.grpc]` /
    /// `.openapi` / `.asyncapi` extraction knobs. Safe to call from any thread.
    pub fn extract_with_config(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        cfg: &ExtractConfig,
    ) -> FileIndex {
        let path_str = file_path.to_string_lossy();
        let lang_kind = LanguageKind::from_path(&path_str);
        let mut out = FileIndex::default();

        // Parses `content` once for `lang_kind` at the indexing budget
        // (`AstGuard::INDEX_PARSE_TIMEOUT_MICROS`) and marks `out.parse_failed` on a
        // real failure — as opposed to `AstGuard::with_parser`'s bare `Option`, which
        // made "the parse failed" and "this language has no grammar" indistinguishable
        // from "it parsed and produced nothing" at every one of these call sites.
        macro_rules! parsed {
            ($f:expr) => {
                match AstGuard::parse_with(
                    lang_kind,
                    content,
                    AstGuard::INDEX_PARSE_TIMEOUT_MICROS,
                    $f,
                ) {
                    ParseOutcome::Parsed(r) => Some(r),
                    ParseOutcome::ParseFailed => {
                        out.parse_failed = true;
                        None
                    }
                    ParseOutcome::NoGrammar => None,
                }
            };
        }

        match lang_kind {
            LanguageKind::Protobuf => {
                if cfg.allows_proto_path(&path_str) {
                    if let Some((nodes, relations)) = parsed!(|tree| {
                        proto::ProtoExtractor::extract_with_relations(
                            file_path,
                            content,
                            repo_id,
                            cfg.canonical_fqcn_projection,
                            tree,
                        )
                    }) {
                        out.nodes = nodes;
                        out.dependencies = relations.dependencies;
                    }
                }
            }

            LanguageKind::Java => {
                if let Some((nodes, dependencies, producers, rpc_calls)) = parsed!(|tree| {
                    java::JavaExtractor::extract_relations(file_path, content, repo_id, tree)
                }) {
                    for (i, node) in nodes.iter().enumerate() {
                        if node.kind == NodeKind::KafkaTopic {
                            out.consumers.push((i, node.name.clone()));
                        }
                    }
                    out.nodes = nodes;
                    out.dependencies = dependencies;
                    out.producers = producers;
                    out.rpc_calls = rpc_calls;
                }
            }
            LanguageKind::Go => {
                if let Some((nodes, relations)) = parsed!(|tree| {
                    go::GoExtractor::extract_with_relations(file_path, content, repo_id, tree)
                }) {
                    out.nodes = nodes;
                    out.dependencies = relations.dependencies;
                    out.producers = relations.producers;
                    out.consumers = relations.consumers;
                    out.rpc_calls = relations.rpc_calls;
                }
            }
            LanguageKind::Python => {
                if let Some((nodes, relations)) = parsed!(|tree| {
                    python::PythonExtractor::extract_with_relations(
                        file_path, content, repo_id, tree,
                    )
                }) {
                    out.nodes = nodes;
                    out.dependencies = relations.dependencies;
                    out.producers = relations.producers;
                    out.consumers = relations.consumers;
                    out.rpc_calls = relations.rpc_calls;
                }
            }
            LanguageKind::TypeScript => {
                let mut imports = Vec::new();
                let mut rpc_calls = Vec::new();
                let nodes = parsed!(|tree| {
                    typescript::TypeScriptExtractor::extract_with_config(
                        file_path,
                        content,
                        repo_id,
                        tree,
                        &mut imports,
                        &mut rpc_calls,
                        &cfg.controller_annotations,
                    )
                })
                .unwrap_or_default();
                out.rpc_calls = rpc_calls;

                if !imports.is_empty() {
                    let content_lines: Vec<&str> = content.lines().collect();
                    for (i, node) in nodes.iter().enumerate() {
                        let body = content_lines
                            .get(node.line_start.saturating_sub(1)..node.line_end)
                            .unwrap_or(&[]);

                        for (_sym, imported) in &imports {
                            if imported.is_empty() {
                                continue;
                            }
                            let is_module_path_entry =
                                imported.contains('/') || imported.starts_with('.');
                            let is_used = is_module_path_entry
                                || body.iter().any(|l| l.contains(imported.as_str()));
                            if is_used {
                                out.dependencies.push((i, CompactStr::new(imported)));
                            }
                        }
                    }
                }
                out.nodes = nodes;
            }
            LanguageKind::Rust => {
                if let Some(index) = parsed!(|tree| {
                    rust_lang::RustExtractor::extract_index(file_path, content, repo_id, tree)
                }) {
                    out.merge(index);
                }
            }
            LanguageKind::Cpp => {
                if let Some(index) = parsed!(|tree| {
                    cpp::CppExtractor::extract_file_index(file_path, content, repo_id, tree)
                }) {
                    out.merge(index);
                }
            }
            LanguageKind::Kotlin => {
                if let Some((nodes, relations)) = parsed!(|tree| {
                    kotlin::KotlinExtractor::extract_with_relations(
                        file_path, content, repo_id, tree,
                    )
                }) {
                    for (i, node) in nodes.iter().enumerate() {
                        if node.kind == NodeKind::KafkaTopic {
                            out.consumers.push((i, node.name.clone()));
                        }
                    }
                    for (i, topic) in relations.producers {
                        out.producers.push((i, topic));
                    }
                    for (i, topic) in relations.consumers {
                        out.consumers.push((i, topic));
                    }
                    out.nodes = nodes;
                }
            }
            LanguageKind::CSharp => {
                if let Some((nodes, relations)) = parsed!(|tree| {
                    csharp::CSharpExtractor::extract_with_relations(
                        file_path, content, repo_id, tree,
                    )
                }) {
                    for (i, topic) in relations.producers {
                        out.producers.push((i, topic));
                    }
                    for (i, topic) in relations.consumers {
                        out.consumers.push((i, topic));
                    }
                    out.nodes = nodes;
                }
            }
            LanguageKind::Ruby => {
                if let Some(nodes) = parsed!(|tree| {
                    ruby::RubyExtractor::extract(file_path, content, repo_id, tree)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Php => {
                if let Some(nodes) = parsed!(|tree| {
                    php::PhpExtractor::extract(file_path, content, repo_id, tree)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Swift => {
                if let Some(nodes) = parsed!(|tree| {
                    swift::SwiftExtractor::extract(file_path, content, repo_id, tree)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Scala => {
                if let Some(nodes) = parsed!(|tree| {
                    scala::ScalaExtractor::extract(file_path, content, repo_id, tree)
                }) {
                    out.nodes = nodes;
                }
            }
            LanguageKind::Yaml => {
                Self::extract_yaml_contracts(file_path, content, repo_id, &mut out, cfg);
            }
            LanguageKind::Unknown => {}
        }

        out
    }

    /// Evaluates user-defined declarative patterns from TOML configuration.
    /// Compiles the regexes on every call — prefer `extract_custom_patterns` with
    /// `CompiledPattern::compile_all` done once per run.
    pub fn apply_custom_patterns(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        patterns: &[CustomPatternConfig],
        graph: &mut ContractGraph,
    ) {
        if patterns.is_empty() {
            return;
        }
        let compiled = CompiledPattern::compile_all(patterns);
        Self::extract_custom_patterns(file_path, content, repo_id, &compiled).apply(graph);
    }

    /// Runs precompiled declarative patterns over `content`. Safe to call from any thread.
    pub fn extract_custom_patterns(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        patterns: &[CompiledPattern],
    ) -> FileIndex {
        let mut out = FileIndex::default();
        if patterns.is_empty() {
            return out;
        }

        let path_str = file_path.to_string_lossy();
        let mut package_name: Option<CompactStr> = None;
        let interned: mesh_core::FilePath = std::sync::Arc::from(file_path);

        for pat in patterns {
            if !pat.matches_path(&path_str) {
                continue;
            }

            // `captures_iter` yields ascending offsets, so the 1-based line of each
            // match is tracked incrementally instead of re-counting from byte 0.
            let mut line_cursor = LineCursor::default();
            for caps in pat.regex.captures_iter(content) {
                let line = caps
                    .get(0)
                    .map_or(1, |m| line_cursor.line_at(content, m.start()));
                let target = caps
                    .get(pat.target_group)
                    .map(|m| m.as_str().trim())
                    .unwrap_or("");
                if target.is_empty() {
                    continue;
                }

                let consumer_name = pat
                    .consumer_group
                    .and_then(|idx| caps.get(idx))
                    .map(|m| m.as_str().trim())
                    .unwrap_or(target);

                let (node_name, kind, signature) = match pat.kind {
                    PatternKind::TopicProducer => (
                        format!("produce:{target}"),
                        NodeKind::EventStream,
                        format!("Producer of {target}"),
                    ),
                    PatternKind::TopicConsumer => (
                        if consumer_name != target {
                            consumer_name.to_string()
                        } else {
                            format!("consume:{target}")
                        },
                        NodeKind::PostProcessor,
                        format!("Consumer of {target}"),
                    ),
                    PatternKind::Saga => (
                        target.to_string(),
                        NodeKind::Saga,
                        format!("Saga: {target}"),
                    ),
                    PatternKind::Rpc => (
                        format!("rpc:{target}"),
                        NodeKind::GrpcMethod,
                        format!("RPC call: {target}"),
                    ),
                };

                let package = package_name
                    .get_or_insert_with(|| mesh_core::detect_service_package(file_path, None))
                    .clone();

                let idx = out.nodes.len();
                out.nodes.push(ContractNode {
                    id: 0,
                    name: CompactStr::new(node_name),
                    kind,
                    file_path: interned.clone(),
                    line_start: line,
                    line_end: line,
                    package,
                    repo_id,
                    signature: Some(CompactStr::new(signature)),
                    docstring: None,
                });

                match pat.kind {
                    PatternKind::TopicProducer => {
                        out.producers.push((idx, CompactStr::new(target)))
                    }
                    PatternKind::TopicConsumer => {
                        out.consumers.push((idx, CompactStr::new(target)))
                    }
                    PatternKind::Rpc => out.rpc_calls.push((idx, CompactStr::new(target))),
                    PatternKind::Saga => {}
                }
            }
        }

        out
    }

    fn extract_yaml_contracts(
        file_path: &Path,
        content: &str,
        repo_id: RepoId,
        out: &mut FileIndex,
        cfg: &ExtractConfig,
    ) {
        let path_str = file_path.to_string_lossy().to_lowercase();
        let interned: mesh_core::FilePath = std::sync::Arc::from(file_path);

        // Check for AsyncAPI spec. `asyncapi_spec_files`, when configured, scopes
        // detection to those files exactly instead of the filename/content sniff.
        let is_asyncapi = if cfg.asyncapi_spec_files.is_empty() {
            path_str.contains("asyncapi") || content.contains("asyncapi:")
        } else {
            cfg.asyncapi_spec_files
                .iter()
                .any(|f| spec_file_matches(&path_str, f))
        };
        if is_asyncapi {
            if let Ok(spec) = serde_yaml::from_str::<spec_shape::AsyncApiShape>(content) {
                let lines = YamlLines::new(content);
                let mut seek = lines.seeker(lines.key_line("channels", 1, usize::MAX));
                for name_str in spec.channels.0.iter().map(String::as_str) {
                    let line = seek.key(name_str).unwrap_or(1);
                    let node = ContractNode {
                        id: 0,
                        name: CompactStr::new(name_str),
                        kind: NodeKind::EventStream,
                        file_path: interned.clone(),
                        line_start: line,
                        line_end: line,
                        package: CompactStr::new("asyncapi"),
                        repo_id,
                        signature: Some(CompactStr::new(format!("channel {name_str}"))),
                        docstring: None,
                    };
                    out.producers
                        .push((out.nodes.len(), CompactStr::new(name_str)));
                    out.nodes.push(node);
                }

                // `infer_string_topics`: beyond the structured `channels` mapping,
                // also pick up a non-standard top-level `topics: [..]` string list.
                if cfg.infer_string_topics {
                    let mut seek = lines.seeker(lines.key_line("topics", 1, usize::MAX));
                    for name_str in spec.topics.0.iter().map(String::as_str) {
                        let line = seek.item(name_str).unwrap_or(1);
                        let node = ContractNode {
                            id: 0,
                            name: CompactStr::new(name_str),
                            kind: NodeKind::EventStream,
                            file_path: interned.clone(),
                            line_start: line,
                            line_end: line,
                            package: CompactStr::new("asyncapi"),
                            repo_id,
                            signature: Some(CompactStr::new(format!("inferred topic {name_str}"))),
                            docstring: None,
                        };
                        out.producers
                            .push((out.nodes.len(), CompactStr::new(name_str)));
                        out.nodes.push(node);
                    }
                }
            }
        }

        // Check for OpenAPI spec. `spec_files`, when configured, scopes detection
        // to those files exactly instead of the filename/content sniff.
        let is_openapi = if cfg.openapi_spec_files.is_empty() {
            path_str.contains("openapi")
                || content.contains("openapi:")
                || content.contains("swagger:")
        } else {
            cfg.openapi_spec_files
                .iter()
                .any(|f| spec_file_matches(&path_str, f))
        };
        if is_openapi {
            if let Ok(spec) = serde_yaml::from_str::<spec_shape::OpenApiShape>(content) {
                let lines = YamlLines::new(content);
                // Pass 1: each path's line, in document order (one forward scan).
                let mut seek = lines.seeker(lines.key_line("paths", 1, usize::MAX));
                let path_lines: Vec<Option<usize>> =
                    spec.paths.0.iter().map(|(p, _)| seek.key(p)).collect();
                // Pass 2: a method is searched only between its path's line and the
                // next located path's line, so every line is scanned O(1) times.
                for (i, (p_str, methods)) in spec.paths.0.iter().enumerate() {
                    let path_line = path_lines[i];
                    let next_path = path_lines[i + 1..].iter().flatten().next().copied();
                    // An unlocated path's methods are not searched (they would
                    // match some other path's `get:`); they take line 1, as before.
                    let mut method_seek = path_line
                        .map(|l| lines.seeker_until(Some(l), next_path.unwrap_or(usize::MAX)));
                    for m_str in methods {
                        let ep_name = format!("{} {}", m_str.to_uppercase(), p_str);
                        let line = method_seek
                            .as_mut()
                            .and_then(|s| s.key(m_str))
                            .or(path_line)
                            .unwrap_or(1);
                        let node = ContractNode {
                            id: 0,
                            name: CompactStr::new(&ep_name),
                            kind: NodeKind::HttpEndpoint,
                            file_path: interned.clone(),
                            line_start: line,
                            line_end: line,
                            package: CompactStr::new("openapi"),
                            repo_id,
                            signature: Some(CompactStr::new(&ep_name)),
                            docstring: None,
                        };
                        out.nodes.push(node);
                    }
                }
            }
        }
    }
}

/// Incremental byte-offset -> 1-based line lookup for ascending offsets.
#[derive(Default)]
struct LineCursor {
    offset: usize,
    line: usize,
}

impl LineCursor {
    fn line_at(&mut self, content: &str, offset: usize) -> usize {
        if offset < self.offset {
            *self = Self::default();
        }
        let end = offset.min(content.len());
        let start = self.offset.min(end);
        self.line += content.as_bytes()[start..end]
            .iter()
            .filter(|b| **b == b'\n')
            .count();
        self.offset = end;
        self.line + 1
    }
}

/// A spec's lines, split once. `serde_yaml` keeps no source positions, so
/// contract nodes read from a parsed spec recover their declaration line
/// textually; a key that cannot be found (flow style, anchors) yields `None`, in
/// which case callers fall back to line 1 and `smart_search` treats the anchor
/// as unverified.
struct YamlLines<'a> {
    lines: Vec<&'a str>,
}

impl<'a> YamlLines<'a> {
    fn new(content: &'a str) -> Self {
        Self {
            lines: content.lines().collect(),
        }
    }

    /// 1-based line of the first `key:` mapping entry (bare, `"key"` or `'key'`)
    /// in lines `[from, until)`.
    fn key_line(&self, key: &str, from: usize, until: usize) -> Option<usize> {
        self.find(from, until, |l| {
            let t = l.trim_start().trim_start_matches("- ");
            let rest = t
                .strip_prefix(key)
                .or_else(|| t.strip_prefix('"')?.strip_prefix(key)?.strip_prefix('"'))
                .or_else(|| t.strip_prefix('\'')?.strip_prefix(key)?.strip_prefix('\''));
            rest.is_some_and(|r| r.starts_with(':'))
        })
    }

    /// 1-based line of the first `- item` sequence entry in lines `[from, until)`.
    fn item_line(&self, item: &str, from: usize, until: usize) -> Option<usize> {
        self.find(from, until, |l| {
            l.trim_start()
                .strip_prefix('-')
                .map(|r| r.trim().trim_matches(|c| c == '"' || c == '\''))
                == Some(item)
        })
    }

    fn find(&self, from: usize, until: usize, pred: impl Fn(&str) -> bool) -> Option<usize> {
        let start = from.saturating_sub(1).min(self.lines.len());
        let end = until.saturating_sub(1).clamp(start, self.lines.len());
        self.lines[start..end]
            .iter()
            .position(|l| pred(l))
            .map(|i| start + i + 1)
    }

    /// A forward-only cursor over entries after `section` (the section key's
    /// line), or from line 1 when the section key itself was not located.
    fn seeker(&self, section: Option<usize>) -> Seeker<'_, 'a> {
        self.seeker_until(section, usize::MAX)
    }

    fn seeker_until(&self, section: Option<usize>, until: usize) -> Seeker<'_, 'a> {
        Seeker {
            lines: self,
            cursor: section.map_or(1, |l| l + 1),
            until,
            misses_left: Seeker::MAX_MISSES,
        }
    }
}

/// Locates a section's entries in document order. Each lookup resumes after the
/// previous hit; a miss leaves the cursor where it was, so one unlocatable entry
/// (an escaped or multi-line key) does not cost the entries after it their lines.
/// A miss scans to the end of the range, so after [`Seeker::MAX_MISSES`] of them
/// the seeker gives up and every later entry falls back to line 1: that bounds a
/// section to `MAX_MISSES + 1` passes over its lines. Re-searching from the top
/// on every entry was O(entries × lines) — quadratic on a large spec, worse for
/// flow-style YAML where every lookup misses and scanned to EOF.
struct Seeker<'l, 'a> {
    lines: &'l YamlLines<'a>,
    cursor: usize,
    until: usize,
    misses_left: u8,
}

impl Seeker<'_, '_> {
    const MAX_MISSES: u8 = 8;

    fn key(&mut self, key: &str) -> Option<usize> {
        self.advance(|lines, from, until| lines.key_line(key, from, until))
    }

    fn item(&mut self, item: &str) -> Option<usize> {
        self.advance(|lines, from, until| lines.item_line(item, from, until))
    }

    fn advance(
        &mut self,
        find: impl FnOnce(&YamlLines<'_>, usize, usize) -> Option<usize>,
    ) -> Option<usize> {
        if self.misses_left == 0 {
            return None;
        }
        let found = find(self.lines, self.cursor, self.until);
        match found {
            Some(line) => self.cursor = line + 1,
            None => self.misses_left -= 1,
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_polyglot_indexer_cpp() {
        let mut graph = ContractGraph::new();
        let cpp = "namespace billing {\nclass Invoice {};\nvoid HandleCharge(int amount) {}\n}\n";
        PolyglotIndexer::index_file(
            Path::new("services/billing/invoice.hpp"),
            cpp,
            0,
            &mut graph,
        );
        assert_eq!(graph.node_count(), 2);
        assert!(graph.all_nodes().any(|n| n.name == "Invoice"));
        assert!(graph
            .all_nodes()
            .any(|n| n.name == "HandleCharge" && n.kind == NodeKind::HttpEndpoint));
    }

    /// Wiring check for items 2/3/5: the per-language extractors gained
    /// relations-aware entry points (`extract_relations`/`extract_with_relations`/
    /// `extract_index`/`extract_file_index`), but `PolyglotIndexer::extract`'s
    /// dispatch is what the real indexing pipeline (`WorkspaceIndexer`) actually
    /// calls. These tests go through `index_file` (which calls the real dispatch),
    /// not the per-language extractor directly, so a regression here means the
    /// feature is unreachable in production even if the language's own unit tests
    /// pass.
    #[test]
    fn test_polyglot_indexer_wires_java_import_dependency() {
        let mut graph = ContractGraph::new();
        PolyglotIndexer::index_file(
            Path::new("services/billing/Widget.java"),
            "package billing;\npublic class Widget {}\n",
            0,
            &mut graph,
        );
        PolyglotIndexer::index_file(
            Path::new("services/orders/Consumer.java"),
            "package orders;\nimport billing.Widget;\npublic class Consumer {\n    void use(Widget w) {}\n}\n",
            0,
            &mut graph,
        );
        assert!(
            !graph.find_dependents("Widget").is_empty(),
            "Java import dependency must reach find_dependents through the real PolyglotIndexer dispatch"
        );
    }

    #[test]
    fn test_polyglot_indexer_wires_go_import_dependency() {
        // Go dependencies are keyed by the raw import path string (Go imports a
        // whole package, not a specific symbol) — mirrors go.rs's own
        // `find_dependents_links_consumer_via_go_import` test.
        let mut graph = ContractGraph::new();
        PolyglotIndexer::index_file(
            Path::new("utils/helper.go"),
            "package utils\n\nfunc Helper() string {\n    return \"ok\"\n}\n",
            0,
            &mut graph,
        );
        PolyglotIndexer::index_file(
            Path::new("main.go"),
            "package main\n\nimport (\n    \"myapp/utils\"\n)\n\nfunc Run() {\n    utils.Helper()\n}\n",
            0,
            &mut graph,
        );
        assert!(
            graph
                .find_dependents("myapp/utils")
                .iter()
                .any(|n| n.name == "Run"),
            "Go import dependency must reach find_dependents through the real PolyglotIndexer dispatch"
        );
    }

    #[test]
    fn test_polyglot_indexer_wires_rust_use_dependency() {
        let mut graph = ContractGraph::new();
        PolyglotIndexer::index_file(
            Path::new("services/billing/widget.rs"),
            "pub struct Widget;\n",
            0,
            &mut graph,
        );
        PolyglotIndexer::index_file(
            Path::new("services/orders/consumer.rs"),
            "use billing::Widget;\n\nfn use_it(_w: Widget) {}\n",
            0,
            &mut graph,
        );
        assert!(
            !graph.find_dependents("Widget").is_empty(),
            "Rust use dependency must reach find_dependents through the real PolyglotIndexer dispatch"
        );
    }

    #[test]
    fn test_polyglot_indexer_wires_cpp_include_dependency() {
        // C++ dependencies are keyed by the #include's file stem, matched against
        // literal usage in the consumer's body — mirrors cpp.rs's own
        // `test_cpp_include_dependency_resolves_via_find_dependents` test, so the
        // header's stem ("Shape") must match the symbol name used in the body.
        let mut graph = ContractGraph::new();
        PolyglotIndexer::index_file(
            Path::new("Shape.hpp"),
            "class Shape {\npublic:\n    virtual ~Shape() = default;\n    virtual double Area() const = 0;\n};\n",
            0,
            &mut graph,
        );
        PolyglotIndexer::index_file(
            Path::new("circle.cpp"),
            "#include \"Shape.hpp\"\n#include <vector>\n\nclass Circle : public Shape {\npublic:\n    double Area() const override { return 3.14; }\n};\n",
            0,
            &mut graph,
        );
        assert!(
            graph
                .find_dependents("Shape")
                .iter()
                .any(|n| n.name == "Circle"),
            "C++ #include dependency must reach find_dependents through the real PolyglotIndexer dispatch"
        );
    }

    #[test]
    fn test_polyglot_indexer_proto() {
        let mut graph = ContractGraph::new();
        let proto = "syntax = \"proto3\"; package test.v1; service TestService { rpc DoTest (Req) returns (Resp); }";
        PolyglotIndexer::index_file(Path::new("test.proto"), proto, 0, &mut graph);
        assert_eq!(graph.node_count(), 2);
    }

    #[test]
    fn test_polyglot_indexer_asyncapi() {
        let mut graph = ContractGraph::new();
        let yaml = r#"
asyncapi: 2.6.0
channels:
  billing.events:
    description: Billing event topic
"#;
        PolyglotIndexer::index_file(Path::new("asyncapi.yaml"), yaml, 0, &mut graph);
        assert_eq!(graph.node_count(), 1);
        let impact = graph.analyze_impact("billing.events");
        assert!(!impact.topics.is_empty());
    }

    /// Line recovery for spec entries: one unlocatable key (an escaped quote
    /// the text search cannot match) must not cost the keys after it their
    /// lines, and an OpenAPI method is only searched inside its own path.
    #[test]
    fn spec_line_recovery_survives_a_miss_and_is_path_bounded() {
        let asyncapi = "asyncapi: 2.6.0\nchannels:\n  'it''s.escaped':\n    description: x\n  orders.created:\n    description: y\n";
        let idx = PolyglotIndexer::extract_with_config(
            Path::new("asyncapi.yaml"),
            asyncapi,
            0,
            &ExtractConfig::default(),
        );
        let line_of = |name: &str| {
            idx.nodes
                .iter()
                .find(|n| n.name.as_str() == name)
                .map(|n| n.line_start)
        };
        assert_eq!(line_of("it's.escaped"), Some(1), "unlocatable key");
        assert_eq!(line_of("orders.created"), Some(5), "key after a miss");

        let openapi = "openapi: 3.0.0\npaths:\n  /a: {get: {}}\n  /b:\n    post: {}\n    get: {}\n";
        let idx = PolyglotIndexer::extract_with_config(
            Path::new("openapi.yaml"),
            openapi,
            0,
            &ExtractConfig::default(),
        );
        let line_of = |name: &str| {
            idx.nodes
                .iter()
                .find(|n| n.name.as_str() == name)
                .map(|n| n.line_start)
        };
        assert_eq!(
            line_of("GET /a"),
            Some(3),
            "flow-style method: its path's line, not /b's get"
        );
        assert_eq!(line_of("POST /b"), Some(5));
        assert_eq!(line_of("GET /b"), Some(6));
    }

    /// Flow-style YAML misses every key; the seeker stops searching after
    /// `MAX_MISSES` instead of rescanning the file once per entry.
    #[test]
    fn seeker_gives_up_after_its_miss_budget() {
        let content = (0..50).map(|i| format!("k{i}: v\n")).collect::<String>();
        let lines = YamlLines::new(&content);
        let mut seek = lines.seeker(None);
        for _ in 0..Seeker::MAX_MISSES {
            assert_eq!(seek.key("absent"), None);
        }
        assert_eq!(seek.key("k3"), None, "budget spent: no more searching");
        let mut fresh = lines.seeker(None);
        assert_eq!(fresh.key("absent"), None);
        assert_eq!(fresh.key("k3"), Some(4), "a miss keeps the cursor");
        assert_eq!(fresh.key("k1"), None, "forward-only");
    }

    #[test]
    fn test_declarative_custom_patterns() {
        let mut graph = ContractGraph::new();
        let patterns = vec![
            CustomPatternConfig {
                name: "outbox_producer".to_string(),
                kind: PatternKind::TopicProducer,
                file_pattern: Some("*.ts".to_string()),
                regex: r#"createEvent<([^>]+)>"#.to_string(),
                target_group: 1,
                consumer_group: None,
            },
            CustomPatternConfig {
                name: "post_processor_consumer".to_string(),
                kind: PatternKind::TopicConsumer,
                file_pattern: Some("*.ts".to_string()),
                regex: r#"class\s+(\w+)\s+extends\s+\w*PostProcessor<([^>]+)>"#.to_string(),
                target_group: 2,
                consumer_group: Some(1),
            },
        ];

        let ts_producer = "await EventQueueEntity.createEvent<UserCreatedEvent>(event);";
        let ts_consumer = "export class UserCreatedPostProcessor extends SinglePostProcessor<UserCreatedEvent> {}";

        PolyglotIndexer::apply_custom_patterns(
            Path::new("services/user/producer.ts"),
            ts_producer,
            1,
            &patterns,
            &mut graph,
        );
        PolyglotIndexer::apply_custom_patterns(
            Path::new("services/user/consumer.ts"),
            ts_consumer,
            1,
            &patterns,
            &mut graph,
        );

        let impact = graph.analyze_impact("UserCreatedEvent");
        assert_eq!(impact.upstream_producers.len(), 1);
        assert_eq!(impact.downstream_consumers.len(), 1);
        assert_eq!(
            impact.downstream_consumers[0].name.as_str(),
            "UserCreatedPostProcessor"
        );
    }

    #[test]
    fn test_proto_dirs_scopes_extraction() {
        let proto = "syntax = \"proto3\"; package test.v1; service TestService { rpc DoTest (Req) returns (Resp); }";

        // Unrestricted (default): a `.proto` file anywhere is extracted.
        let default_cfg = ExtractConfig::default();
        let unrestricted = PolyglotIndexer::extract_with_config(
            Path::new("services/other/test.proto"),
            proto,
            0,
            &default_cfg,
        );
        assert_eq!(unrestricted.nodes.len(), 2);

        // `proto_dirs` configured: files outside every configured dir are not
        // extracted as protobuf at all.
        let scoped_cfg = ExtractConfig {
            proto_dirs: vec!["proto-registry".to_string()],
            ..ExtractConfig::default()
        };
        let outside = PolyglotIndexer::extract_with_config(
            Path::new("services/other/test.proto"),
            proto,
            0,
            &scoped_cfg,
        );
        assert!(outside.nodes.is_empty());

        let inside = PolyglotIndexer::extract_with_config(
            Path::new("proto-registry/test.proto"),
            proto,
            0,
            &scoped_cfg,
        );
        assert_eq!(inside.nodes.len(), 2);
    }

    #[test]
    fn test_openapi_asyncapi_spec_files_scopes_detection() {
        let openapi_yaml = r#"
paths:
  /users:
    get:
      summary: list users
"#;
        // File neither named nor containing an "openapi"/"swagger" sniff hint,
        // so the default (unconfigured) sniffing misses it entirely.
        let default_cfg = ExtractConfig::default();
        let missed = PolyglotIndexer::extract_with_config(
            Path::new("services/billing/api-contract.yaml"),
            openapi_yaml,
            0,
            &default_cfg,
        );
        assert!(missed.nodes.is_empty());

        // `spec_files` configured to name this exact file: now it is scoped in,
        // regardless of filename/content sniffing.
        let scoped_cfg = ExtractConfig {
            openapi_spec_files: vec!["api-contract.yaml".to_string()],
            ..ExtractConfig::default()
        };
        let scoped = PolyglotIndexer::extract_with_config(
            Path::new("services/billing/api-contract.yaml"),
            openapi_yaml,
            0,
            &scoped_cfg,
        );
        assert_eq!(scoped.nodes.len(), 1);
        assert!(scoped.nodes.iter().any(|n| n.name == "GET /users"));
    }

    #[test]
    fn test_infer_string_topics_toggle() {
        let yaml = r#"
asyncapi: 2.6.0
channels:
  billing.events:
    description: Billing event topic
topics:
  - legacy.orders.created
"#;
        // Default: `infer_string_topics = true` picks up both the structured
        // `channels` entry and the non-standard `topics:` string list.
        let default_cfg = ExtractConfig::default();
        let with_inference =
            PolyglotIndexer::extract_with_config(Path::new("asyncapi.yaml"), yaml, 0, &default_cfg);
        assert_eq!(with_inference.nodes.len(), 2);
        assert!(with_inference
            .nodes
            .iter()
            .any(|n| n.name == "legacy.orders.created"));

        // Disabled: only the structured `channels` entry is extracted.
        let disabled_cfg = ExtractConfig {
            infer_string_topics: false,
            ..ExtractConfig::default()
        };
        let without_inference = PolyglotIndexer::extract_with_config(
            Path::new("asyncapi.yaml"),
            yaml,
            0,
            &disabled_cfg,
        );
        assert_eq!(without_inference.nodes.len(), 1);
        assert!(!without_inference
            .nodes
            .iter()
            .any(|n| n.name == "legacy.orders.created"));
    }

    #[test]
    fn test_extract_config_from_contracts() {
        use mesh_core::config::{AsyncApiConfig, ContractsConfig, GrpcConfig, OpenApiConfig};

        let contracts = ContractsConfig {
            enabled: true,
            grpc: Some(GrpcConfig {
                proto_dirs: vec!["proto-registry".to_string()],
                controller_annotations: vec!["@RpcHandler".to_string()],
                canonical_fqcn_projection: false,
            }),
            spring: None,
            openapi: Some(OpenApiConfig {
                enabled: true,
                spec_files: vec!["openapi.yaml".to_string()],
            }),
            asyncapi: Some(AsyncApiConfig {
                enabled: true,
                spec_files: vec!["asyncapi.yaml".to_string()],
                infer_string_topics: false,
            }),
            cpp: None,
            patterns: Vec::new(),
        };

        let cfg = ExtractConfig::from_contracts(&contracts);
        assert_eq!(cfg.proto_dirs, vec!["proto-registry".to_string()]);
        assert_eq!(cfg.controller_annotations, vec!["@RpcHandler".to_string()]);
        assert!(!cfg.canonical_fqcn_projection);
        assert_eq!(cfg.openapi_spec_files, vec!["openapi.yaml".to_string()]);
        assert_eq!(cfg.asyncapi_spec_files, vec!["asyncapi.yaml".to_string()]);
        assert!(!cfg.infer_string_topics);
    }

    /// Pattern / AsyncAPI / OpenAPI nodes carry their real declaration line, not
    /// a placeholder `1` (smart_search anchors snippets on it).
    #[test]
    fn declarative_nodes_record_real_lines() {
        let patterns = CompiledPattern::compile_all(&[CustomPatternConfig {
            name: "producer".to_string(),
            kind: PatternKind::TopicProducer,
            file_pattern: None,
            regex: r#"send\("([^"]+)"\)"#.to_string(),
            target_group: 1,
            consumer_group: None,
        }]);
        let src = "a\nb\nsend(\"t.one\")\nc\n\nsend(\"t.two\")\n";
        let out = PolyglotIndexer::extract_custom_patterns(Path::new("x.java"), src, 0, &patterns);
        let lines: Vec<(String, usize)> = out
            .nodes
            .iter()
            .map(|n| (n.name.to_string(), n.line_start))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("produce:t.one".to_string(), 3),
                ("produce:t.two".to_string(), 6)
            ]
        );

        let asyncapi = "asyncapi: 2.6.0\ninfo:\n  title: x\nchannels:\n  billing.events:\n    description: d\n  'orders.created':\n    description: e\n";
        let out = PolyglotIndexer::extract(Path::new("asyncapi.yaml"), asyncapi, 0);
        let lines: Vec<(String, usize)> = out
            .nodes
            .iter()
            .map(|n| (n.name.to_string(), n.line_start))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("billing.events".to_string(), 5),
                ("orders.created".to_string(), 7)
            ]
        );

        let openapi = "openapi: 3.0.0\npaths:\n  /orders:\n    get:\n      summary: list\n    post:\n      summary: create\n  /users:\n    get:\n      summary: u\n";
        let out = PolyglotIndexer::extract(Path::new("openapi.yaml"), openapi, 0);
        let mut lines: Vec<(String, usize)> = out
            .nodes
            .iter()
            .map(|n| (n.name.to_string(), n.line_start))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            vec![
                ("GET /orders".to_string(), 4),
                ("GET /users".to_string(), 9),
                ("POST /orders".to_string(), 6)
            ]
        );
    }
}
