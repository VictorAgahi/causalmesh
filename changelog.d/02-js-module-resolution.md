### JS/TS parser: module aliases (7.1.0 stack, PR 2/7)
- **`tsconfig.json` / `jsconfig.json` `paths`, `baseUrl` and relative `extends` chains** and
  **`package.json` `imports`** (`#internal/*`, conditional targets) are resolved: an aliased import
  (`import User from '@/services/User'`, `require('#lib/log')`) is rewritten to the relative path
  the file would have written, so the graph resolves it. Before, a default/namespace/`require`
  import of an alias named no symbol and produced no edge (named imports resolved by bare name only).
- **A relative import from a JS/TS file now links only to the file it names** (graph, `mesh-core`):
  before, `./utils` matched any same-named symbol in the repo. The specifier is resolved against the
  importer's directory (extension, `index` and component-folder aware: `./x.js` -> `x.ts`,
  `./services` -> `services/index.ts`, `./Button` -> `Button/Button.tsx`). On the otel-demo, react and
  vscode corpora this removed 659 / 57,129 / 4.6M `Imports` edges, sampled and classified as false
  positives (an import of `../types/Cart` linking a Go file, `./ReactSharedInternals` linking another
  package's file, ...) and added 57 correct ones on otel-demo (formerly ambiguous edges now pointing at a
  single file). Only *module* imports are path-aware: a named specifier (`import { util } from
  './util'`) is still resolved by its bare name, as before, so homonyms stay ambiguous for those.
  A directory import resolved through `package.json` `"main"` no longer links. Other languages'
  relative imports are unchanged.
- Longest-literal-prefix wins (TypeScript's rule); exact aliases match only themselves; a catch-all
  (`"*": [...]`) never claims bare packages; a package `extends` (`@tsconfig/node18`) and
  `baseUrl`-only bare specifiers are not resolved; configs are never read from above the file's root: every read is confined to the owning workspace root
  (symlinks and `..`/absolute `extends` are resolved first and refused when they leave it; the size cap
  applies to the bytes read), and a file under no root is never looked up.
- The nearest `tsconfig.json`/`jsconfig.json` (and the nearest `package.json`) governs a file even when it
  declares no aliases, so a nested package does not inherit its parent's. `extends` may name a directory.
- JSONC is parsed properly (comments, trailing commas, non-ASCII).
- Lookup is lazy per directory and per extraction config (no workspace-wide discovery pass, no
  global state). A script's index-cache key folds in the aliases that govern it, so editing a
  `tsconfig.json` re-extracts exactly the files under it on the next cold start. Editing an alias
  config in a running daemon reloads the scripts it governs, so an incremental reload equals a full
  rebuild (invariant I2): a `tsconfig*.json` / `jsconfig*.json` (any name, since another config may
  `extends` it from anywhere) governs its whole root, a `package.json` its directory. The daemon keeps a
  digest of what each config last held (the whole file for a tsconfig, the `imports` map for a
  `package.json`): an edit that cannot move an alias (a version bump, a dependency) re-extracts
  nothing, while a config appearing, vanishing or edited for the first time since the daemon started
  does, once.
- The extraction-config fingerprint no longer depends on the workspace roots (adding a root does not
  invalidate the whole cache).
- A FIFO or special file named like a config is skipped (it used to block the scan forever).
- Replaces the unused `TsConfigResolver`. Only the first `paths` target is used.
