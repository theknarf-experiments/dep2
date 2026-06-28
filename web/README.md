# dep2 — live import graph

A small React SPA that visualizes a project's import/dependency graph as a live,
force-directed graph rendered in WebGL. It polls the dep2 engine's read-only
query API and redraws as the code (and therefore the relations) change.

**Stack**

- [TanStack DB](https://tanstack.com/db) — reactive store. Each engine relation
  is a collection; a query-backed sync polls the API and diffs rows by key, so
  live queries update incrementally (mirroring the engine's own model). See
  `db.ts` / `useGraphData.ts`.
- [React Three Fiber](https://r3f.docs.pmnd.rs/) + [three.js](https://threejs.org/)
  — our own renderer (`ForceGraph.tsx`): nodes as meshes, edges as one
  `lineSegments` with direction-gradient color, instanced arrowheads, drei text
  labels, node-drag, hover-to-focus, pan/zoom, auto-fit.
- [d3-force](https://github.com/d3/d3-force) — the layout simulation, run in a
  Web Worker (`forceWorker.ts`) that posts node positions back to the renderer.

The HUD (`Hud.tsx`) is a plain DOM overlay above the canvas — its buttons get
native clicks and the empty areas are pointer-transparent so the graph still
pans/zooms behind it.

## Tests

Playwright drives the real app (it boots the engine over `./crates` and the Vite
dev server itself):

```sh
pnpm test:e2e
```

It asserts the graph renders, the Modules/Files toggle changes the node set,
pause works, and the console stays error-free. A screenshot is written to
`test-results/graph.png`.

### Measuring frame rate

The renderer holds a locked 120fps even on large graphs (thousands of nodes) on
a real GPU. **Don't** read fps from the headless Playwright run: headless
Chromium renders with SwiftShader (a software rasterizer), so its fps reflects
CPU rasterization, not the GPU the app actually runs on — a big instanced graph
can read ~13fps there while the same page is pinned at 120fps in any real
browser. To verify fps against the actual GPU, run the headed probe:

```sh
URL=http://localhost:5173/ node tests/gpufps.mjs
```

It launches a headed Chromium (real GPU), prints the WebGL renderer string, and
samples fps while the layout settles, at steady state, during a pan, and in the
module view.

## Run it

The easy way — from the repo root, start the engine and this UI together:

```sh
mise run graph            # analyze ./crates
mise run graph some/dir   # analyze another tree
```

Then open the URL Vite prints (usually <http://localhost:5173>). Ctrl+C stops
both the engine and the dev server.

### Manually

First-time setup (grammars, deps, build) is `mise run setup` from the repo root
(see the root README). Then:

```sh
# 1. engine (serves the relations with CORS enabled)
dep2 run examples/import_graph.dl \
  --source 'treesitter:root=crates;grammars=rs=./grammars/tree-sitter-rust.wasm,toml=./grammars/tree-sitter-toml.wasm,json=./grammars/tree-sitter-json.wasm' \
  --addr 127.0.0.1:7878

# 2. web UI
pnpm -C web dev
```

## What it shows

The engine runs [`examples/import_graph.dl`](../examples/import_graph.dl). Modules
are derived from project manifests — a Cargo.toml `[package] name` or a
package.json `name` — not from path heuristics, so the graph is language-agnostic.
A workspace (from a Cargo workspace / pnpm-workspace.yaml) links its member
modules. Import edges come from the AST (Rust `use`, JS/TS/MDX `import`/`export …
from`, `require()`, dynamic `import()`, and Vite `import.meta.glob("./pattern")`
expanded to the matching files). Relative imports are resolved by actual path,
and `tsconfig.json` `compilerOptions.paths` aliases (e.g. `~/* -> ./src/*`) are
read and resolved to real files. MDX and Markdown files are parsed with their
own tree-sitter grammars. Six relations are exposed:

| relation                    | meaning                                              |
| --------------------------- | ---------------------------------------------------- |
| `module_node(module)`       | every module (a manifest's declared name)            |
| `module_edge(from, to)`     | module → module dependency                           |
| `workspace_node(ws)`        | a workspace grouping modules                         |
| `workspace_link(ws, module)`| workspace → member module                            |
| `file_node(file, module)`   | every source file and its owning module              |
| `file_link(src, dst)`       | intra-project file → file import (module tree, JS imports) |

- **Modules** view: one node per module plus the workspace; edges are
  cross-module dependencies and workspace membership.
- **Files** view: one node per file (colored by module); edges are the
  file → file imports — the Rust module tree (`mod foo;`), intra-crate
  `use crate::` / `super::`, and JS/TS relative/aliased imports.

Toggle the granularity (**Modules**/**Files**) or **Pause** from the toolbar; the
FPS / worst-frame meter is there too. Interactions:

- **Pan**: two-finger trackpad scroll (or drag empty space).
- **Zoom**: pinch, or Ctrl/⌘+scroll — centered on the cursor.
- **Drag** a node to reposition it; **hover** to focus its neighborhood.
- **Click** a node for an info panel (path, module, what it imports / is imported
  by); click empty space to dismiss.

The graph auto-fits on first paint and on view switch.

## Data view

The **Graph** / **Data** / **Rules** switch in the toolbar flips between views.

The **Data** view (`DataView.tsx`, built on [TanStack Table](https://tanstack.com/table))
lists every relation the engine serves with live row counts, and shows the
selected relation's rows in a sortable, filterable table (`useRawData.ts` polls
`/relations` and `/relations/<name>`, respecting Pause). Known relations get
friendly column headers; any other relation gets positional ones — so it works
for any `.dl` program, not just the import graph.

The **Rules** view (`RulesView.tsx`) shows the `.dl` program loaded into the
engine, fetched from the `/program` endpoint. It renders the source with line
numbers and Datalog syntax highlighting (a small tokenizer in `dlHighlight.ts`
colors comments, strings, relations, builtins, operators, and types),
a rule + declaration summary, and a find box that highlights matches — handy for
seeing exactly which rules are running.

## Rendering backends

The graph always renders through **R3F / three.js** (`ForceGraph` from
`@dep2/force-graph`) — it owns the camera, pan/zoom/drag/hover, labels and
raycasting. The **force layout** runs on the **GPU via WebGPU** when available: a
verified-exact port of d3-force (see the package's `verify-gpu-oracle`) that
plugs in behind the same protocol the d3-force Web Worker uses, so it looks like
the CPU layout but computes on the GPU. If WebGPU is unavailable (or init fails)
the layout transparently falls back to the **d3-force Web Worker**, off the main
thread. Either way the rendering and interaction are identical.

The graph is computed incrementally by the engine, so edits to the analyzed
source show up within a poll interval — no restart.

## Config

- `VITE_DEP2_API` — API base URL (otherwise `http://127.0.0.1:7878`).
- Poll interval defaults to 1500 ms (`config` in `db.ts`).
