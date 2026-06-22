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

## GPU layout (WebGPU, experimental)

For very large graphs the d3-force worker (CPU) is the bottleneck. `GpuLayout`
(`src/gpu/sim.ts`) is a WebGPU compute force-sim that keeps positions in a GPU
buffer the whole time — so a renderer can bind that buffer directly with no CPU
round-trip, which is what makes millions of nodes viable.

It runs the whole step on the GPU: bin nodes into a coarse uniform grid, repel
every node from the occupied cell centroids (a mass-weighted particle-mesh, so
repulsion is O(cells·n) not O(n²)), apply springs over edges, centre, and
integrate with velocity damping and a cooling `alpha`. It's pure WebGPU (no DOM),
so it runs in the browser and in Deno for headless verification.

```ts
import { GpuLayout } from "@dep2/force-graph";
const device = await (await navigator.gpu.requestAdapter()).requestDevice();
const sim = new GpuLayout({ device, nodeCount, edges /* Uint32Array [s,t,...] */ });
sim.step(1);                  // advance; call per frame
sim.positions;               // live GPUBuffer ([x,y] per node) to render from
const xy = await sim.readPositions(); // CPU copy (tests/export only)
```

Verified headless on Apple M1 Pro (Metal) via Deno's WebGPU — `mise run verify-gpu`:
it checks the layout converges on a known grid mesh (connected nodes end up far
closer than random pairs, no NaN, settles) and benchmarks scale:

| nodes | ms / step | steps/s |
| ----: | --------: | ------: |
| 10k | ~1.1 | ~940 |
| 100k | ~2.6 | ~380 |
| 1M | ~22 | ~45 |

(For comparison, the tuned CPU d3-force is ~21 ms/*tick* at 10k and can't do 1M.)

`GpuRenderer` (`src/gpu/render.ts`) renders straight from the sim's position
buffer — edges as lines, nodes as instanced circle quads — plus an integer
"pick" pass (node index → texture, one texel read identifies the node under the
cursor). It's device-agnostic (renders into any texture view), so `verify-gpu`
also draws to an offscreen texture and asserts pixels + a pick land.

`GpuForceGraph` is the React component that ties them together: owns a `<canvas>`,
runs sim + render in a rAF loop, and handles pan / zoom / drag / hover / select —
same prop shape as `ForceGraph`, with an `onUnsupported` callback to fall back
when WebGPU is missing.

```tsx
import { GpuForceGraph } from "@dep2/force-graph";
<GpuForceGraph elements={elements} hovered={h} setHovered={setH}
  selected={s} setSelected={setS} activeGroup={g} perf={perfRef}
  onUnsupported={() => useWebglFallback()} />
```

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
