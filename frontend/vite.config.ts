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
        replacement: decodeURIComponent(new URL("./src/stubs/echarts.ts", import.meta.url).pathname),
      },
      // See src/lib/shiki-slim.ts — rnui's CodeBlock imports bare `shiki`,
      // whose bundle registers every grammar/theme (~12 MB of chunks, all
      // embedded into the argus binary). The shim registers bash + the two
      // default themes only. Exact match on purpose: the shim itself imports
      // `shiki/core` and `shiki/engine/javascript`, which must NOT be rewritten.
      {
        find: /^shiki$/,
        replacement: decodeURIComponent(new URL("./src/lib/shiki-slim.ts", import.meta.url).pathname),
      },
    ],
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Measured floor after echarts was stubbed out: ~679 kB (gzip ~224 kB), and
    // what remains is react-dom + rnui/base-ui, not bloat. Vite's 500 kB default
    // would warn on every build forever, so it is raised to just above the real
    // number — low enough that genuine new bloat still trips it. Lower this (or
    // split vendor chunks) if the app is ever served over a slow link; today it
    // ships embedded in the control-plane binary and is served over the LAN.
    chunkSizeWarningLimit: 750,
  },
  server: {
    // During `npm run dev`, proxy the API/stream surfaces to the local control
    // plane so the SPA and backend share an origin.
    //
    // `/api` MUST use the object form with `ws: true`: the terminal is a
    // WebSocket (`/api/machines/:id/terminal`), and the shorthand string form
    // does not forward upgrade requests — the socket just never opens. That
    // fails silently in a way that looks like a broken terminal rather than a
    // broken proxy, because xterm does not echo locally (the remote PTY does),
    // so a dead socket shows no prompt AND no response to typing.
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
