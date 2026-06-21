import { defineConfig, devices } from "@playwright/test";

// Boots the dep2 engine (over ./crates) and the Vite dev server, then drives the
// SPA in a real browser. The engine command builds dep2 first so a fresh
// checkout works; both reuse an already-running instance if present.
export default defineConfig({
  testDir: "./tests",
  timeout: 90_000,
  expect: { timeout: 40_000 },
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL: "http://localhost:5173",
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: [
    {
      command:
        'cargo build -q -p dep2 && exec target/debug/dep2 run examples/import_graph.dl --source "treesitter:root=crates;grammars=rs=./grammars/tree-sitter-rust.wasm" --addr 127.0.0.1:7878',
      cwd: "..",
      url: "http://127.0.0.1:7878/relations",
      timeout: 180_000,
      reuseExistingServer: true,
      stdout: "ignore",
      stderr: "pipe",
    },
    {
      command: "npm run dev",
      url: "http://localhost:5173",
      timeout: 60_000,
      reuseExistingServer: true,
    },
  ],
});
