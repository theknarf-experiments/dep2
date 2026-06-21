import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The SPA polls the dep2 query API directly (CORS is enabled server-side), so no
// dev proxy is needed. The API base URL is configurable in the UI; it defaults
// to VITE_DEP2_API or http://127.0.0.1:7878.
export default defineConfig({
  plugins: [react()],
  server: { port: 5173 },
});
