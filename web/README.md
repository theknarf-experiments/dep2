# dep2 — live import graph

A small React + [cytoscape](https://js.cytoscape.org/) SPA that visualizes a
project's import/dependency graph as a live, force-directed graph. It polls the
dep2 engine's read-only query API and redraws as the code (and therefore the
relations) change.

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
cd web && npm install && npm run dev
```

## What it shows

The engine runs [`examples/import_graph.dl`](../examples/import_graph.dl), which
derives import edges from the AST (Rust `use` and JS/TS `import`/`export ... from`)
and exposes five relations:

| relation                 | meaning                                       |
| ------------------------ | --------------------------------------------- |
| `crate_node(crate)`      | every Rust workspace crate                    |
| `crate_edge(from, to)`   | crate → crate internal `use` dependencies     |
| `file_node(file, group)` | every source file and its owning crate/dir    |
| `file_edge(file, crate)` | Rust file → workspace crate it imports         |
| `file_import(src, dst)`  | JS/TS file → sibling file it imports          |

- **Crates** view: one node per Rust crate, edges are the internal dependency
  graph.
- **Files** view: one node per file (colored by crate/dir). Rust files point at
  the crates they import; JS/TS files point at the sibling files they import
  (relative imports resolved by basename within the same directory group).

Toggle the granularity, point it at a different engine with the **API** field,
adjust the poll interval, or pause. Click a node to focus its neighborhood.

The graph is computed incrementally by the engine, so edits to the analyzed
source show up within a poll interval — no restart.

## Config

- `VITE_DEP2_API` — default API base URL (otherwise `http://127.0.0.1:7878`).
  Can also be changed live in the UI.
