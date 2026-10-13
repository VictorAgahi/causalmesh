# MeshMCP Architectural Authority Rule

MeshMCP is an AST-based index across the workspace roots, not a text search. Its answers are a
verifiable starting point: every row carries a `path:line` you can open.

## Key Principles for Coding Agents

1. **A zero is scoped, never absolute.**
   When `analyze_grpc`, `analyze_impact` or `find_dependents` returns:
   - `0 (no literal call sites found in the indexed roots)` / `0 (no literal registrations found in the indexed roots)`
   - `No dependents found`
   - `No producer statically resolved` / `No consumer statically resolved` (the topic or the other side is indexed, but no literal call site was found: the name may be built at runtime from a registry, a config or a template string, or the producer may live in a root that is not indexed)

   it means no literal match in the indexed roots. Treat it as strong evidence for what the index can
   see, and say so ("not found in `<roots>` with `<tool>`"). A claim of absence ("never called",
   "no producer") needs a query across ALL roots with the tool whose coverage matches the question.
   A targeted check of a dynamic name, a registry or an unindexed root is worth doing; a broad
   `grep` re-scan of the whole workspace rarely adds anything.

2. **Rows marked `heuristic` or `ambiguous`, and notes about unresolved, dynamic or depth-limited
   hops** (`Depth limit reached`, `Chain ends`), deserve a targeted read of the `path:line` given.

3. **Tool Selection Guidance**:
   - Use `analyze_grpc` for Protobuf definitions, gRPC services, server handlers, client stubs, and
     "who calls this RPC".
   - Use `analyze_impact` for distributed events, message brokers, topics, consumers, multi-hop
     saga traces (`depth`), and the handler behind an HTTP route (`VERB /path`).
   - Use `find_dependents` for cross-package and cross-service reverse dependencies (imports and
     symbol usage), not for RPC callers.
   - Use `smart_search` for AST-decapitated symbol declarations and HTTP routes.
   - Use `ripgrep` for plain unstructured text, string literals inside function bodies, or to verify
     a line a tool already pointed to.
