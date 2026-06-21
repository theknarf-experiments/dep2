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
- [d3-force](https://github.com/d3/d3-force) — the layout simulation, ticked
  manually inside the R3F render loop.

The HUD (`Hud.tsx`) is a plain DOM overlay above the canvas — its buttons get
native clicks and the empty areas are pointer-transparent so the graph still
pans/zooms behind it.

## Tests

Playwright drives the real app (it boots the engine over `./crates` and the Vite
dev server itself):

```sh
pnpm test:e2e
```

It asserts the graph renders, the Crates/Files toggle changes the node set, pause
works, and the console stays error-free. A screenshot is written to
`test-results/graph.png`.

## Run it

The easy way — from the repo root, start the engine and this UI together:

```sh
mise run graph            # analyze ./crates
mise run graph some/dir   # analyze another tree
```

Then open the URL Vite prints (usually <http://localhost:5173>). Ctrl+C stops
both the engine and the dev server.

### Manually

```sh
# 1. engine (serves the edge relations with CORS enabled)
dep2 run examples/import_graph.dl \
  --source 'treesitter:root=crates;grammars=rs=./grammars/tree-sitter-rust.wasm' \
  --addr 127.0.0.1:7878

# 2. web UI
pnpm install && pnpm -C web dev
```

## What it shows

The engine runs [`examples/import_graph.dl`](../examples/import_graph.dl), which
derives import edges from the AST (Rust `use` and JS/TS `import`/`export ... from`)
and exposes five relations:

| relation                 | meaning                                       |
| ------------------------ | --------------------------------------------- |
| `crate_node(crate)`      | every Rust workspace crate                    |
| `crate_edge(from, to)`   | crate → crate internal `use` dependencies     |
| `file_node(file, group)` | every source file and its owning crate/dir              |
| `file_edge(file, crate)` | Rust file → external workspace crate it imports         |
| `file_link(src, dst)`    | intra-project file → file (module tree, intra-crate use, JS imports) |

- **Crates** view: one node per Rust crate, edges are the internal dependency
  graph.
- **Files** view: one node per file (colored by crate/dir). Files point at the
  external crates they import (`file_edge`) and at the project files they depend
  on (`file_link`): the Rust module tree (`mod foo;`), intra-crate `use crate::`
  / `super::`, and JS/TS relative imports.

Toggle the granularity (**Crates**/**Files**) or **Pause** from the in-scene
toolbar. Drag to pan, scroll to zoom, drag a node to reposition it, and hover a
node to focus its neighborhood. The graph auto-fits on first paint and on view
switch.

The graph is computed incrementally by the engine, so edits to the analyzed
source show up within a poll interval — no restart.

## Config

- `VITE_DEP2_API` — API base URL (otherwise `http://127.0.0.1:7878`).
- Poll interval defaults to 1500 ms (`config` in `db.ts`).
