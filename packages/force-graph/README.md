# @dep2/force-graph

A reusable React Three Fiber force-directed graph: instanced nodes/edges with
arrowheads, drei text labels, d3-force layout in a Web Worker, and built-in
pan / zoom / drag / hover / select. Extracted from the dep2 web UI so it can be
developed and demoed on its own (Storybook).

Consumed as **TypeScript source** (the `exports` point at `src/index.ts`), so the
worker (`new URL("./forceWorker.ts", import.meta.url)`) is bundled by the
consumer's Vite — no build step or dual-package hazard.

## Use

```tsx
import { ForceGraph, GraphElements } from "@dep2/force-graph";

const elements: GraphElements = {
  nodes: [{ id: "a", label: "A", color: "#61afef", group: "g1", radius: 8, alwaysLabel: true }],
  edges: [{ id: "a->b", source: "a", target: "b" }],
};

// Inside your own <Canvas>:
<ForceGraph
  elements={elements}
  hovered={hovered} setHovered={setHovered}
  selected={selected} setSelected={setSelected}
  activeGroup={activeGroup}   // dim everything outside this node.group
  layoutKey={view}            // changing it re-fits the view
  perf={perfRef}              // optional fps/worst-frame readout
/>
```

`ForceGraphCanvas` is the same component wrapped in its own `<Canvas>` for
standalone use (and Storybook).

`GraphNode` carries presentation directly — `color`, optional `group`, `radius`,
`alwaysLabel`, `fontSize` — so the renderer stays domain-agnostic. `colorFor(name)`
is a handy deterministic name→HSL helper.

## Develop

```sh
pnpm -C packages/force-graph storybook        # dev (http://localhost:6006)
pnpm -C packages/force-graph build-storybook  # static build
pnpm -C packages/force-graph typecheck
# or, from the repo root:
mise run storybook
```

`react`, `react-dom`, `three`, `@react-three/fiber`, and `@react-three/drei` are
peer dependencies (provided by the consumer so there's a single `three`).
