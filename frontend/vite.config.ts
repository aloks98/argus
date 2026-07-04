import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Builds to dist/, which the Rust control plane embeds via rust-embed
// (crates/server/src/embed.rs) and serves with SPA fallback (PRD §10).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
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
