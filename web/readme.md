# podseq.site

Landing page + documentation site for the Podseq framework, built with
SvelteKit and fully prerendered to static HTML. Docs are rendered directly from
`../docs/src/**/*.md`, so the markdown stays the single source of truth.

## Stack

- SvelteKit + Svelte 5 (runes), `@sveltejs/adapter-static`
- Every route is prerendered; deploys as plain static files
- Tailwind CSS v4, `marked` for markdown

## Routes

- `/`: landing page
- `/docs/`: docs introduction
- `/docs/<slug>/`: a doc page (e.g. `architecture`, `components/core`)

Legacy hash URLs (`#/docs/<slug>~<heading>`) redirect to the new paths via a
small script in `src/app.html`, so old links keep working.

## How docs are loaded

`src/docs` is a symlink to `../docs`. The docs module (`src/lib/docs.ts`)
globs `src/docs/src/**/*.md` at build time and renders each page with
`marked`; heading anchors, in-app links, and copy buttons are emitted by
custom renderers (no DOM post-processing, so it also runs during
prerendering). The sidebar is built from `SUMMARY.md`, and
`src/routes/docs/[...slug]` prerenders every slug via `entries()`. Editing any
file under `../docs/src` is reflected on the next reload. No build or copy
step.

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

Deploy: upload the `dist/` directory to any static host.

> The `src/docs` symlink must exist. If it is missing, recreate it from the
> `web` directory: `ln -s ../../docs src/docs`.
