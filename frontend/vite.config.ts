import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Builds to dist/, which the Rust control plane embeds via rust-embed
// (crates/server/src/embed.rs) and serves with SPA fallback (PRD §10).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // No stub aliases anymore: rnui >= 0.2 tree-shakes (sideEffects:false +
  // preserved modules) so unused chart components take echarts with them,
  // and CodeBlock takes an explicit highlighter (lib/codeHighlighter.ts)
  // instead of statically referencing shiki's full grammar/theme maps —
  // the full-bundle behavior is an opt-in import (code-block-full) nothing
  // here uses. If dist ever sprouts hundreds of grammar chunks again,
  // something reintroduced a full-bundle reference.
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Measured floor post-code-splitting (2026-08-01): entry ~921 kB (gzip
    // ~288 kB) of react-dom + the rnui barrel — the pages, xterm, logviewer
    // and uplot are lazy chunks now, so the entry can't shrink further from
    // this repo's side. The remaining fat is rnui not tree-shaking (no
    // `sideEffects` field, single flat dist bundle + module-scope
    // registrations); fix that upstream in rnui, then lower this. Set just
    // above the floor so genuine new entry bloat still warns.
    chunkSizeWarningLimit: 1000,
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
