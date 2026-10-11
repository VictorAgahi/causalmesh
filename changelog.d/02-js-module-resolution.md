### JS/TS parser: module aliases (7.1.0 stack, PR 2/7)
- **`tsconfig.json` / `jsconfig.json` `paths`, `baseUrl` and relative `extends` chains** and
  **`package.json` `imports`** (`#internal/*`, conditional targets) are resolved: an aliased import
  (`import User from '@/services/User'`, `require('#lib/log')`) is rewritten to the relative path
  the file would have written, so the graph resolves it. Before, a default/namespace/`require`
  import of an alias named no symbol and produced no edge (named imports resolved by bare name only).
- Longest-literal-prefix wins (TypeScript's rule); exact aliases match only themselves; a catch-all
  (`"*": [...]`) never claims bare packages; a package `extends` (`@tsconfig/node18`) and
  `baseUrl`-only bare specifiers are not resolved; configs are never read from above the file's root.
- JSONC is parsed properly (comments, trailing commas, non-ASCII).
- Lookup is lazy per directory and per extraction config (no workspace-wide discovery pass, no
  global state). A script's index-cache key folds in the aliases that govern it, so editing a
  `tsconfig.json` re-extracts exactly the files under it on the next cold start. Unchanged files are
  not re-extracted by an incremental reload when only a `tsconfig.json` changes: restart for that.
- Replaces the unused `TsConfigResolver`.
