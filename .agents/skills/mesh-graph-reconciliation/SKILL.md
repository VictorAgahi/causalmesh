---
name: mesh-graph-reconciliation
description: >-
  Use when changing the MeshMCP contract graph: node or edge kinds, the secondary
  indices, reconcile_edges, patch_files, or the query methods behind find_dependents,
  analyze_grpc, analyze_impact and smart_search.
---

# MeshMCP Graph Reconciliation Skill

The cross-service graph lives in
[`crates/mesh-core/src/contracts.rs`](../../../crates/mesh-core/src/contracts.rs) and is
published as one field of a `MeshSnapshot`. It is built by the indexing pipeline and read
by every MCP tool.

---

## 1. Quick Navigation & Codebase References

- **Core data types**: [`crates/mesh-core/src/types.rs`](../../../crates/mesh-core/src/types.rs)
  - `CompactStr = compact_str::CompactString`
  - `RepoId = u16` — interned repository index, up to 65 535 roots; `RepoId::MAX` is a
    sentinel for synthetic cross-repo nodes
  - `NodeId = u32`, `EdgeId = u32`
  - `FilePath = Arc<Path>` — interned, one buffer per file
  - `ContractNode { id, name, kind, file_path, line_start, line_end, package, repo_id, signature, docstring }`
  - `NodeKind`: `GrpcService`, `GrpcMethod`, `HttpEndpoint`, `KafkaTopic`, `EventStream`,
    `Queue`, `ProtoMessage`, `PostProcessor`, `Saga`, `ServiceClass`, `Interface`
  - `EdgeKind`: `Produces`, `Consumes`, `CallsRpc`, `Implements`, `Imports`, `DispatchesTo`
  - `CanonicalMethodId::new(package, service, method)` → `package.Service/Method`,
    `to_pascal_case()`, `detect_service_package()`
- **Graph engine**: [`crates/mesh-core/src/contracts.rs`](../../../crates/mesh-core/src/contracts.rs)
  - build: `add_node`, `add_edge`, `add_dependency`, `add_producer`, `add_consumer`, `add_rpc_call`
  - maintain: `patch_file`, `patch_files`, `reconcile_edges`
  - query: `find_dependents`, `analyze_grpc`, `analyze_impact`, `search_symbols`,
    `get_nodes_for_file`, `get_node`, `all_nodes`, `all_edges`, `node_count`, `edge_count`
  - results borrow: `GrpcTrace<'g>`, `ImpactFlow<'g>` hold `Vec<&'g ContractNode>`
- **Snapshot / state**: [`crates/mesh-core/src/state.rs`](../../../crates/mesh-core/src/state.rs)
  - `MeshSnapshot { contract_graph, doc_index, property_registry, generation }`
  - `AppState { config, allowed_roots, governance, snapshot: ArcSwap<MeshSnapshot>, audit, rescan, vfs, reload_pending }`
  - read: `state.snapshot()`; publish: `state.install_snapshot(snap)`; mutate-a-copy: `state.snapshot_clone()`
- **Who builds it**: [`crates/mesh-server/src/indexer.rs`](../../../crates/mesh-server/src/indexer.rs) —
  see [`mesh-indexing-pipeline`](../mesh-indexing-pipeline/SKILL.md)
- **Rendering**: [`crates/mesh-parsers/src/graph.rs`](../../../crates/mesh-parsers/src/graph.rs)
  (`GraphRenderer::to_mermaid` / `to_json` / `to_html`, `WebGraphPayload`)

---

## 2. State: one snapshot, not a graph behind a lock

```rust
pub struct AppState {
    pub config: Arc<Config>,
    pub allowed_roots: Arc<[PathBuf]>,
    pub governance: Arc<GovernanceEngine>,
    pub snapshot: ArcSwap<MeshSnapshot>,
    pub audit: Arc<AuditLogger>,
    pub rescan: Arc<BackgroundRescanEngine>,
    pub vfs: Mutex<DifferentialVfs>,
    pub reload_pending: AtomicBool,
}
```

Only `snapshot` is hot-swapped. The graph, the doc index and the property registry travel
together inside `MeshSnapshot` so a reader can never see a new graph beside a stale doc
index — which separate per-field `ArcSwap`s allowed. `generation` is a monotonic counter
bumped by `install_snapshot`, letting a caller detect a reload.

```rust
let snapshot = state.snapshot();                       // guard, lock-free
let dependents = snapshot.contract_graph.find_dependents(target);
```

```rust
let mut snapshot = state.snapshot_clone();             // mutate a copy
snapshot.contract_graph.patch_files(stale);
snapshot.contract_graph.reconcile_edges();
let generation = state.install_snapshot(snapshot);     // atomic publish
```

Never take a `RwLock` over the graph, never hold a snapshot guard across an `.await`, and
never mutate a snapshot that has been installed.

---

## 3. Indices: what exists and what it is keyed by

```rust
nodes: HashMap<NodeId, ContractNode>,
edges: Vec<ContractEdge>,

name_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
package_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
file_to_nodes: HashMap<FilePath, Vec<NodeId>>,
fqcn_to_node: HashMap<CompactStr, NodeId>,
reverse_deps: HashMap<CompactStr, Vec<NodeId>>,
topic_producers: HashMap<CompactStr, Vec<NodeId>>,
topic_consumers: HashMap<CompactStr, Vec<NodeId>>,
rpc_calls: Vec<(NodeId, CompactStr)>,
```

`add_node` is the only place that populates them, and it does three things worth knowing:
it assigns `id` from `next_node_id` (so `NodeId`s are fold-order dependent and not stable
across generations); it registers `package/Name` in `fqcn_to_node` for `GrpcMethod` and
`GrpcService`; and for `KafkaTopic` / `EventStream` / `Queue` it *additionally* indexes the
node under its **lowercased** name in `name_to_nodes`, which is how topic keys line up with
`topic_producers` / `topic_consumers`.

Any new index must be populated in `add_node` and purged in `patch_files`, or a reload
leaks stale `NodeId`s.

---

## 4. `reconcile_edges`: one pass, after the whole fold

Run exactly once per snapshot, after every `FileIndex` has been applied. Five stages, in
order, per its doc comment:

1. Resolve or drop placeholder import edges. `add_dependency` pushes an `Imports` edge
   with `to: 0` and the import string in `metadata`; reconcile resolves it via
   `resolve_import_target(from, target)` and **drops** it when unresolvable — that is how
   external packages like `@nestjs/common` disappear instead of pointing at node 0.
2. & 3. Topic hubs and causal dispatch: for each key in `topic_producers ∪ topic_consumers`,
   find or synthesise the hub node, then wire `Produces` / `Consumes` and, when a topic has
   both, `DispatchesTo` from each producer to each consumer.
4. `Implements`: service handlers to their protobuf RPC declarations.
5. `CallsRpc`: client call sites to proto methods.

De-duplication is a set, seeded once from the existing edges:

```rust
let mut edge_set: HashSet<(NodeId, NodeId, EdgeKind)> =
    self.edges.iter().map(|e| (e.from, e.to, e.kind)).collect();
```

`resolve_import_target` tries, in order: an exact name hit in `name_to_nodes`; for a
relative or absolute path, the file stem restricted to the importer's own `repo_id`; then
the qualified package identifier via `package_to_nodes`. Every lookup is an index hit —
`nodes.values().find(...)` in this function is a regression.

The synthesised topic hub is worth reading before you touch it:

```rust
let node = ContractNode {
    id: 0,
    name: topic_key.clone(),
    kind: NodeKind::EventStream,
    file_path: FilePath::from(Path::new("event-bus")),
    ...
    repo_id: RepoId::MAX,
    signature: Some(CompactStr::new(format!("topic://{topic_key}"))),
    docstring: None,
};
```

Two deliberate choices: the kind is `EventStream`, not `KafkaTopic`, because declarative
`[[engines.contracts.patterns]]` hits are transport-agnostic (only extractors that really
detect Kafka tag `KafkaTopic`); and `repo_id` is `RepoId::MAX`, a sentinel no real root
index can reach, because `0` would silently attribute every topic in the mesh to whichever
root happens to be first in `[workspace] roots`.

---

## 5. Incremental maintenance: `patch_files`

```rust
pub fn patch_files<'a>(&mut self, file_paths: impl IntoIterator<Item = &'a Path>)
```

Collects the stale `NodeId`s from `file_to_nodes` for every given path and removes them
from all the secondary indices in a single pass, instead of one full `retain` sweep per
file. `patch_file` is the single-path wrapper. Call it for changed **and** deleted files
before folding the new fragments in — `WorkspaceIndexer::reload` does exactly that.

---

## 6. Queries

- `find_dependents(target)` — `reverse_deps` direct hit; if empty, falls back to scanning
  the `reverse_deps` keys for a substring match (package-level). Returns `Vec<&ContractNode>`.
- `analyze_grpc(target)` — `GrpcTrace<'_>` with `proto_definition`, `client_stubs`,
  `server_handlers`. Matches `GrpcService` / `GrpcMethod` nodes by name (ASCII
  case-insensitive) or by FQCN containment; `is_proto_file` decides what counts as the
  definition.
- `analyze_impact(target)` — `ImpactFlow<'_>` with `upstream_producers`, `topics`,
  `downstream_consumers`, `related_sagas`.
- `search_symbols(query, scope_filter)` — exact-name fast path through `name_to_nodes`
  first, then a substring walk restricted to files under `scope_filter` via
  `file_to_nodes`, de-duplicated with a `HashSet<NodeId>`, exact hits first. Case
  insensitivity uses the allocation-free `contains_ignore_ascii_case`, never
  `to_lowercase()`. This is the index that makes `smart_search` index-first.

All of these borrow from the graph. Hold the snapshot guard for the duration; do not clone
nodes to escape a lifetime.

---

## 7. Invariants

1. One `reconcile_edges` per snapshot, after the fold. Never per file.
2. Every index is populated in `add_node` and purged in `patch_files`.
3. No linear scan over `nodes` or `edges` in a query or in reconcile.
4. `NodeId` is not stable across generations — resolve by name, package or file.
5. Topic keys are lowercase at both write and read.
6. Synthetic nodes carry `repo_id: RepoId::MAX`.
7. Adding an `EdgeKind` or `NodeKind` means updating `reconcile_edges`, the affected
   `MarkdownFormatter::format_*`, and `GraphRenderer` — a new variant that renders as
   nothing is worse than no variant.

## 8. In-depth reference

Lock-free snapshot mechanics, FQCN canonicalization across languages, and blast-radius
traversal: [`DEEPENING.md`](DEEPENING.md).
