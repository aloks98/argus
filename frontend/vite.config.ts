import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Builds to dist/, which the Rust control plane embeds via rust-embed
// (crates/server/src/embed.rs) and serves with SPA fallback (PRD §10).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: [
      // See src/stubs/echarts.ts — drops ~55% of the bundle. Remove once rnui
      // no longer registers echarts at module scope.
      {
        find: /^echarts(\/.*)?$/,
        replacement: decodeURIComponent(
          new URL("./src/stubs/echarts.ts", import.meta.url).pathname,
        ),
      },
      // See src/lib/shiki-slim.ts — shim for bare `shiki` (rnui's CodeBlock
      // import), which otherwise pulls ~12 MB of grammar/theme chunks.
      // Exact match on purpose: must not also rewrite the shim's own
      // `shiki/core` / `shiki/engine/javascript` imports.
      {
        find: /^shiki$/,
        replacement: decodeURIComponent(
          new URL("./src/lib/shiki-slim.ts", import.meta.url).pathname,
        ),
      },
    ],
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Measured floor post-echarts-stub: ~679 kB (gzip ~224 kB) of react-dom +
    // rnui, not bloat — raised just above that so genuine new bloat still
    // trips the default 500 kB warning. Lower this if ever served off-LAN.
    chunkSizeWarningLimit: 750,
  },
  server: {
    // During `npm run dev`, proxy the API/stream surfaces to the local control
    // plane so the SPA and backend share an origin.
    //
    // `/api` MUST use the object form with `ws: true`: the shorthand string
    // form silently drops upgrade requests, breaking the terminal WebSocket
    // (`/api/machines/:id/terminal`) — a dead socket with no prompt, no error.
    //
    // Dev-only: the production build is embedded in the control-plane binary
    // and served from the same origin, so no proxy is involved there.
    proxy: {
      "/api": { target: "http://localhost:8080", ws: true },
      // The OIDC flow redirects the BROWSER to /auth/*, so the dev server must
      // proxy it too -- otherwise vite's SPA fallback serves index.html and the
      // login silently renders the app shell instead of redirecting.
      "/auth": { target: "http://localhost:8080" },
    },
  },
});
