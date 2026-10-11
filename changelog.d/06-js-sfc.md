### JS/TS parser: single-file components (7.1.0 stack, PR 6/7)
- **`.vue`, `.svelte` and `.astro` files are indexed.** Their code is their `<script>` blocks (Vue
  `<script>` + `<script setup>`, Svelte `context="module"` + instance, Astro `---` frontmatter + client
  `<script>`s) read with the JS / TS grammar (`lang="ts"` / `"tsx"`, TypeScript for the Astro
  frontmatter). Everything outside the blocks is blanked byte for byte, newlines kept, so declarations,
  imports and broker calls report the **line numbers of the original file**.
- **A component is a declaration named after its file** (`Dashboard.vue` -> `Dashboard`, `index.vue` -> its
  directory), spanning the file and owning its imports: `import Card from './Card.vue'` links to the
  component (the `.vue` / `.svelte` / `.astro` extension is part of the module key), including through
  `tsconfig` / `package.json` aliases. An options-API `export default` is that declaration (no duplicate).
- **Pages by location**: SvelteKit `+page.svelte` and Astro `src/pages/**/*.astro` are `PAGE` routes, even
  with no script.
- The script gets the lexical guard of any parsed source (nesting depth, line length) measured on the script alone: long template / style lines (inline SVG, data URIs, Tailwind lists) do not hide it.
- The block scanner is one linear pass over the markup: it skips comments and `<style>`, every other tag with its quoted attribute values (`<div :title="'<script>'">` holds no script), and reads attributes case-insensitively. It skips `<script>` mentioned in comments and `<style>`, self-closing external scripts, reads quoted attributes containing `>` (`generic="T extends Array<string>"`) and ignores non-JS blocks (`lang="coffee"`, `type="application/ld+json"`, import maps); the Astro frontmatter may follow a BOM or blank lines.
- An explicit `.vue` / `.svelte` / `.astro` import names only such a file, and an explicit `.js` / `.ts` never names a component (`Foo.vue` beside `Foo.ts`).
- `+page` / `+layout` files are routes, not named components; SvelteKit pages need `src/routes`, Astro skips `_`-prefixed files and folders.
- The script gets the same lexical guard as any parsed source (nesting depth, line length). The watcher
  reloads on `.vue` / `.svelte` / `.astro` edits and alias-config edits re-extract them.
- **Not covered**: template-only component usage (auto-imported components, `<UserCard />` with no import),
  Nuxt `pages/*.vue` (needs a Nuxt signal the file does not carry), `<script src="…">` external scripts,
  decapitation and `smart_search` snippets of components (the file is shown as written).
