import tailwindcss from "@tailwindcss/vite";
import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const here = fileURLToPath(new URL(".", import.meta.url));

// `src/docs` symlinks to ../../docs, so allow the repo root for the dev server.
export default defineConfig({
    plugins: [
        tailwindcss(),
        sveltekit({
            adapter: adapter({ pages: "dist", assets: "dist" }),
            preprocess: vitePreprocess(),
        }),
    ],
    server: {
        fs: {
            allow: [here, `${here}..`],
        },
    },
});
