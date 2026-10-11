### JS/TS parser: file-system routing (7.1.0 stack, PR 5/7)
- **The location of a module is its route** for the file-system routers, as `HttpEndpoint` nodes named and
  signed `METHOD /path` (`PAGE /path` for a UI page), anchored to the export that declares them:
  Next.js App Router (`app/**/route.ts` verbs, `page.tsx`; `(group)`, `@slot`, `[id]`, `[...slug]`,
  `[[...slug]]`), Next.js Pages Router (`pages/**`, `pages/api/**` as `ALL`), Remix / React Router flat
  routes (`users.$id.tsx`, `_index`, pathless `_auth.login`, the `route.tsx` folder form; `loader` is `GET`,
  `action` is `POST`), SvelteKit (`+server.ts` verbs, `+page(.server).ts` load / `actions`), Nuxt / Nitro
  (`server/api/**` under `/api`, `server/routes/**`, `x.get.ts` method suffixes) and Astro endpoints
  (`src/pages/**` verb exports).
- **A directory name is not evidence**: each convention also needs a signal from the file (an import of the
  framework, `import type` included, or its handler helpers such as `defineEventHandler`, or its route
  exports). A CRA-style `src/pages/Home.tsx`, a `routes/helpers.ts`, a `lib/app/util.ts` are not routes.
- **Not in this PR**: `.vue` / `.svelte` / `.astro` pages (next PR), TanStack Router / Expo Router / Qwik City /
  SolidStart conventions, `middleware`, parallel/intercepting route details beyond ignoring `@slot`.
