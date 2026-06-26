# podseq.site

Landing page + documentation renderer for the Podseq framework. Docs are rendered
directly from `../docs/src/**/*.md`, so the markdown stays the single source of truth.

## Stack

- Svelte 5 (runes), client-side rendered
- Vite 6 + Tailwind CSS v4
- `marked` for markdown

## How docs are loaded

`src/docs` is a symlink to `../docs`. The docs module (`src/lib/docs.ts`) globs
`src/docs/src/**/*.md` at build time, parses each page with `marked`, rewrites
inter-document `.md` links into in-app routes, and builds the sidebar from
`SUMMARY.md`. Editing any file under `../docs/src` is reflected on the next
reload. No build or copy step.

Routing is hash-based:

- `#/`: landing page
- `#/docs`: docs introduction
- `#/docs/<slug>`: a doc page (e.g. `architecture`, `components/core`)
- `#/docs/<slug>~<heading-id>`: scroll to a heading

## Develop

```sh
cd web
bun install     # or: npm install
bun run dev     # http://localhost:5173
```

## Build

```sh
bun run build       # outputs dist/
bun run preview     # serve the build
bun run check       # svelte-check (types)
```

> The `src/docs` symlink must exist. If it is missing, recreate it from the
> `web` directory: `ln -s ../../docs src/docs`.
