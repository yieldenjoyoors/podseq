import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));

// `src/docs` symlinks to ../../docs, so allow the repo root for the dev server.
export default defineConfig({
  plugins: [tailwindcss(), svelte()],
  server: {
    fs: {
      allow: [here, `${here}..`],
    },
  },
});
