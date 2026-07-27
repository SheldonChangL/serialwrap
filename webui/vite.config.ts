import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// The default port the daemon's own axum server listens on
// (`serialwrapd::web::DEFAULT_PORT`) — kept in sync manually since this
// config has no access to Rust source. `npm run dev` proxies `/api` there
// so the Vite dev server can be used for frontend iteration without a
// release build; production serving is entirely the daemon's job (see
// `webui/README.md`), this proxy is dev-only.
const DAEMON_DEV_PORT = 5590;

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: {
      "/api": {
        target: `http://127.0.0.1:${DAEMON_DEV_PORT}`,
        ws: true,
      },
    },
  },
  build: {
    // Embedded in a single binary (rust-embed) — keep the output flat and
    // predictable rather than chasing marginal caching gains from more
    // aggressive chunking.
    outDir: "dist",
    emptyOutDir: true,
  },
});
