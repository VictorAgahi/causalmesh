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
    (variant kept for `mesh-parsers::graph`'s render match arms; `reconcile_edges` no longer
    constructs it — see section 4)
  - `EdgeConfidence`: `Exact` (FQCN/fully-qualified match, or a structural edge derived from
    node identity), `Heuristic` (bare-name, case-insensitive or substring match) and
    `Ambiguous` (several candidates tied; one edge per candidate).
    `ContractEdge.confidence` carries it; `MarkdownFormatter` and `GraphRenderer` surface it
    so an agent can weigh a result instead of treating every edge as fact.
  - `CanonicalMethodId::new(package, service, method)` → `package.Service/Method`,
    `to_pascal_case()`, `detect_service_package()`
- **Graph engine**: [`crates/mesh-core/src/contracts.rs`](../../../crates/mesh-core/src/contracts.rs)
  - build: `add_node`, `add_edge`, `add_dependency`, `add_producer`, `add_consumer`, `add_rpc_call`
  - maintain: `patch_file`, `patch_files`, `reconcile_edges`
  - query: `find_dependents`, `analyze_grpc`, `analyze_impact`, `analyze_impact_with_depth`,
    `impact_matrix`, `search_symbols`, `get_nodes_for_file`, `get_node`, `all_nodes`,
    `all_edges`, `node_count`, `edge_count`
  - determinism: `canonical_lines`, `canonical_lines_rooted`, `fingerprint` (backs
    `mesh-mcp graph --format fingerprint` and `scripts/determinism.sh`)
  - results borrow: `GrpcTrace<'g>` (`(&'g ContractNode, EdgeConfidence)` pairs),
    `ImpactFlow<'g>`, `ImpactMatrix<'g>` / `ImpactRow<'g>`
- **Snapshot / state**: [`crates/mesh-core/src/state.rs`](../../../crates/mesh-core/src/state.rs)
  - `MeshSnapshot { contract_graph, doc_index, property_registry, generation, health, roots, .. }`
  - `AppState { config, allowed_roots, governance, snapshot: ArcSwap<MeshSnapshot>, audit, rescan,
    vfs, reload_pending, pending_reload_paths, reload_lock, search_cache, index_cache }`
  - read: `state.snapshot()`; publish: `state.install_snapshot(snap)`; mutate-a-copy: `state.snapshot_clone()`
- **Who builds it**: [`crates/mesh-server/src/indexer.rs`](../../../crates/mesh-server/src/indexer.rs) —
  see [`mesh-indexing-pipeline`](../mesh-indexing-pipeline/SKILL.md)
- **Rendering**: [`crates/mesh-parsers/src/graph.rs`](../../../crates/mesh-parsers/src/graph.rs)
  (`GraphRenderer::to_mermaid` / `to_json` / `to_html`, `WebGraphPayload`) and
  `crates/mesh-parsers/src/topology.rs` (per-service aggregated view behind `visualize_mesh`)

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
    pub pending_reload_paths: Mutex<Vec<PathBuf>>,
    pub reload_lock: Mutex<()>,
    pub search_cache: SearchCache,
    pub index_cache: OnceLock<PersistentIndexCache>,
    // ...
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
nodes: BTreeMap<NodeId, ContractNode>,   // BTreeMap: iteration order must not depend on a random hasher
edges: Vec<ContractEdge>,

name_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
package_to_nodes: HashMap<CompactStr, Vec<NodeId>>,
file_to_nodes: HashMap<FilePath, Vec<NodeId>>,
fqcn_to_node: HashMap<CompactStr, Vec<NodeId>>,   // several nodes may share an FQCN
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

Run exactly once per snapshot, after every `FileIndex` has been applied. It **clears
`self.edges` and rebuilds every edge** from the raw fact indices (`reverse_deps`,
`topic_producers`, `topic_consumers`, `rpc_calls`, node kinds), which `patch_files` keeps
current. That is what makes an incremental reload converge to the same graph as a full
rebuild (`derive(derive(g)) == derive(g)`): an unchanged file's edge to a re-indexed target is
recomputed, never left stale. Five stages, in order:

1. Imports: `add_dependency` only records the fact in `reverse_deps`. Reconcile resolves every
   (target, importer) pair, sorted, via `resolve_import_targets` (memoized per target and
   importer repo) and emits one `Imports` edge per winner, or one `Ambiguous` edge per tied
   candidate; an unresolvable target (an external package such as `@nestjs/common`) produces
   no edge.
2. & 3. Topic hubs and dispatch: for each key in `topic_producers ∪ topic_consumers`, find or
   synthesise the hub node, then wire `Produces` (`producer -> topic`) / `Consumes`
   (`topic -> consumer`), both tagged `Exact` (they derive from producer/consumer
   registration, not name matching). **`DispatchesTo` is no longer generated here** — it used
   to materialize one direct edge per `(producer, consumer)` pair, which is
   `O(producers * consumers)` per topic (a 50×50 hub topic alone produced 2,500 edges,
   growing with the square). The same information is available via the
   two-hop `producer -> topic -> consumer` walk through `Produces`/`Consumes`. Note the
   direction when traversing: `Consumes` edges point topic → consumer
   (`analyze_impact_with_depth` follows whichever end is new, fixed in plan 4 step 4.6a). Do not resurrect direct dispatch edges without a cap; if you need them, bound the
   pair count per topic and document the behaviour at the limit.
4. `Implements`: service handlers to their protobuf RPC declarations, tagged `Exact` for an
   FQCN match, `Heuristic` for a case-insensitive/PascalCase/substring match.
5. `CallsRpc`: client call sites to proto methods (`resolve_rpc_target`), same confidence split;
   `grpc.health.v1.Health` is filtered as infrastructure.

De-duplication is a set, started empty after the clear:

```rust
self.edges.clear();
let mut edge_set: HashSet<(NodeId, NodeId, EdgeKind)> = HashSet::new();
```

`resolve_import_targets` tries, in order: (1) a fully-qualified name — Java `a.b.C`, Rust
`a::b::C`, or `package/Name` — split via `split_fully_qualified` and matched against an
package+name pair or `fqcn_to_node`, tagged `Exact`; (2) an exact bare name hit in
`name_to_nodes`, tagged `Heuristic` (two unrelated types sharing a bare name would collide);
(3) for a relative or absolute path, the file stem restricted to the importer's own
`repo_id`, `Heuristic`; (4) the qualified package identifier via `package_to_nodes`,
`Heuristic`. Every lookup is an index hit — `nodes.values().find(...)` in this function is a
regression.

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
before folding the new fragments in — `WorkspaceIndexer::apply_incremental` (behind both
`reload` and `reload_paths`) does exactly that.

---

## 6. Queries

- `find_dependents(target)` — three steps, each only tried if the previous found nothing:
  1. `reverse_deps` direct hit on `target` as a literal import-path/package string.
  2. Symbol bridge: resolve `target` via `name_to_nodes`/`fqcn_to_node` (it may be a
     declared contract name, not an import string — e.g. a `GrpcService`'s own name),
     take each declaring node's `.package`, and re-query `reverse_deps` with that.
  3. Last-resort unscoped substring scan over every `reverse_deps` key. Can span multiple
     services that share a locally-aliased package name; callers should group the
     returned nodes by `ContractNode::repo_id` before presenting them (the
     `find_dependents` MCP tool does this — see `mesh-tool-authoring`).
  Returns `Vec<&ContractNode>`.
- `analyze_grpc(target)` — `GrpcTrace<'_>` with `proto_definition`, `client_stubs`,
  `server_handlers`. Matches `GrpcService` / `GrpcMethod` nodes by name (ASCII
  case-insensitive) or by FQCN containment; `is_proto_file` decides what counts as the
  definition. The immediate per-node client/server path-substring bucketing
  (`"controller"`/`"handler"`/`"service"` in the path) only runs for an **exact** name
  match — a heuristic (non-exact) name/FQCN match is only surfaced via a real
  `Implements`/`CallsRpc` graph edge to the proto definition below, never by path alone
  (a directory merely named `*service` is not evidence of anything on its own — every
  service in a typical microservices repo satisfies it).
- `analyze_impact(target)` / `analyze_impact_with_depth(target, depth)` — `ImpactFlow<'_>` with
  `upstream_producers`, `topics`, `downstream_consumers`, `related_sagas`.
- `impact_matrix(target, depth)` — `ImpactMatrix<'_>` behind the `analyze_impact` MCP tool: gRPC
  handlers/clients (through `Implements`/`CallsRpc`) plus the async flow, one `ImpactRow` per
  element, classified `EXTERNAL`/`INTERNAL` against the contract's owner roots, deduplicated and
  totally ordered. Rules in `docs/mcp-tools.md` (Tool 4).
- `search_symbols(query, scope_filter)` — exact-name fast path through `name_to_nodes`
  first, then a substring walk restricted to files under `scope_filter` via
  `file_to_nodes`, de-duplicated with a `HashSet<NodeId>`, exact hits first. Case
  insensitivity uses the allocation-free `contains_ignore_ascii_case`, never
  `to_lowercase()`. This is the index that makes `smart_search` index-first.

All of these borrow from the graph. Hold the snapshot guard for the duration; do not clone
nodes to escape a lifetime.

---

## 7. Invariants

1. One `reconcile_edges` per snapshot, after the fold. Never per file. It rebuilds all edges;
   never add an edge outside it that it would not recompute.
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
