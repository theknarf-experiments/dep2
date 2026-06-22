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

## GPU layout (WebGPU)

`GpuLayout` (`src/gpu/sim.ts`) is a **GPU port of d3-force** — not an
approximation of it. Each step reproduces d3's tick exactly: many-body charge,
then links (using the post-charge predicted velocity), then a weak
forceX/forceY centering, then `vx = (vx + forces) * velocityDecay; x += vx`,
with velocity carried across ticks and a cooling `alpha`. Every WGSL pass cites
the d3 source line it implements, and the default parameters match the app's
previous d3 setup (charge −240, link distance 38 / strength 0.45, centering
0.045, velocityDecay 0.4). Positions live in a GPU buffer the whole time, so a
renderer binds that buffer directly with no CPU round-trip.

Repulsion is currently the **exact all-pairs sum** (identical to
`d3.forceManyBody().theta(0)`), which is O(n²) — fast to tens of thousands of
nodes. Barnes-Hut (O(n log n), matching d3's default `theta`) is the planned
scaling step; the exact all-pairs sim is the oracle it will be verified against.

It's pure WebGPU (no DOM), so it runs in the browser and in Deno for headless
verification.

```ts
import { GpuLayout } from "@dep2/force-graph";
const device = await (await navigator.gpu.requestAdapter()).requestDevice();
const sim = new GpuLayout({ device, nodeCount, edges /* Uint32Array [s,t,...] */ });
sim.step(1);                  // advance; call per frame
sim.positions;               // live GPUBuffer ([x,y] per node) to render from
const xy = await sim.readPositions(); // CPU copy (tests/export only)
```

Verified headless on Apple M1 Pro (Metal) via Deno's WebGPU. Two checks:

- `mise run verify-gpu-oracle` — **exactness against d3-force itself** (the
  oracle). With no links the charge + centering + integrator are bit-exact vs
  stock d3 (relative error ~1e-6); the link force matches d3's equations exactly
  in parallel form; and the full system converges to stock d3's actual layout
  (edge-length distribution within a few percent, pairwise-distance Spearman
  ~0.97). The only difference from stock d3 is that links relax in parallel
  (Jacobi) rather than serially (Gauss-Seidel) — unavoidable on a GPU, and both
  converge to the same layout.
- `mise run verify-gpu` — converges on a known grid mesh, disconnected
  components separate, warm restart preserves a settled layout, the renderer
  draws + picks, and a scale benchmark:

| nodes | ms / step | steps/s |
| ----: | --------: | ------: |
| 2k  | ~1.8 | ~560 |
| 10k | ~3.7 | ~270 |
| 30k | ~18  | ~57  |

(O(n²) all-pairs; for comparison the tuned CPU d3-force is ~21 ms/*tick* at 10k.
Barnes-Hut is what will take this to millions.)

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
