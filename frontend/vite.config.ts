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
    proxy: {
      "/api": "http://localhost:8080",
      "/auth": "http://localhost:8080",
    },
  },
});
