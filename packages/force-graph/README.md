# @dep2/force-graph

A reusable React Three Fiber force-directed graph: instanced nodes/edges with
arrowheads, drei text labels, and built-in pan / zoom / drag / hover / select.
The force layout runs on the GPU (WebGPU) when available — a verified-exact port
of d3-force — and falls back to d3-force in a Web Worker otherwise. Extracted
from the dep2 web UI so it can be developed and demoed on its own (Storybook).

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

The renderer and all interaction stay on R3F (`ForceGraph` above). Only the
**force layout** runs on the GPU — that was the bottleneck, not drawing — and it
does so behind the *same message protocol* as the d3-force worker, so `ForceGraph`
uses either backend without any change to its rendering or interaction code.
`ForceGraph` picks the GPU backend when WebGPU is available and falls back to the
d3-force worker otherwise (toggle with the `gpuLayout` prop, default on).

```tsx
// Inside your own <Canvas>: GPU layout when available, d3-force worker otherwise.
<ForceGraph elements={elements} gpuLayout /* default */ ... />
```

`GpuLayout` (`src/gpu/sim.ts`) is a **GPU port of d3-force** — not an
approximation of it. Each step reproduces d3's tick exactly: many-body charge,
then links (using the post-charge predicted velocity), then a weak
forceX/forceY centering, then `vx = (vx + forces) * velocityDecay; x += vx`,
with velocity carried across ticks and a cooling `alpha`. Every WGSL pass cites
the d3 source line it implements (charge −240, link distance 38, link strength
d3's default 1/min(deg), centering 0.045, velocityDecay 0.4). The degree-
normalized link strength is also what keeps the *parallel* relaxation stable —
a constant strength only survives d3's serial Gauss-Seidel; in parallel a
high-degree node sums many simultaneous corrections and diverges (see
`test/gpu-stability.ts`). `GpuLayoutBackend` (`src/gpu/layoutBackend.ts`) wraps it in
the worker protocol (set / drag / dragEnd → tick); positions are read back per
frame to drive the R3F instanced mesh.

Repulsion has two backends, chosen by size (`chargeMode: "auto"` by default,
Barnes-Hut above ~4096 nodes; force `"exact"`/`"bh"` to override):

- **exact** — the all-pairs sum, identical to `d3.forceManyBody().theta(0)`. O(n²),
  bit-exact vs d3, used for small graphs and as the verification oracle.
- **Barnes-Hut** — a quadtree built on the GPU and traversed per node with d3's
  exact θ criterion (`w*w/theta2 < dist2`) and the same charge force law. O(n log n),
  this is what scales to millions. The tree is a regular pyramid built without
  pointers or sorting (nothing that can deadlock under WGSL's memory model):
  scatter each node into its finest cell with atomics, reduce centre-of-mass/mass
  up the levels, then each node walks the pyramid from the root applying θ.

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
- `mise run verify-gpu-bh` — **Barnes-Hut vs the exact path**: per-step force
  within θ tolerance (~2-3%), converged layout matching (Spearman ~0.99, edge
  lengths within 0.1%), and the scale benchmark below.
- `mise run verify-gpu-stability` — the layout stays bounded where d3 does (no
  explosions) on hubs/dense/complete graphs.
- `mise run verify-gpu` — converges on a known grid mesh, disconnected
  components separate, warm restart preserves a settled layout, the renderer
  draws + picks.

Scale (Apple M1 Pro / Metal, Barnes-Hut, ms/step while settling):

| nodes | ms / step | steps/s |
| ----: | --------: | ------: |
| 100k | ~7  | ~140 |
| 1M   | ~54 | ~19  |
| 2M   | ~139 | ~7  |
| 4M   | ~324 | ~3  |

(The settle runs a few hundred steps then stops; exact all-pairs is ~21 ms/*tick*
in CPU d3-force at 10k and can't reach these sizes at all.)

`GpuRenderer` (`src/gpu/render.ts`) is a standalone WebGPU renderer that draws
straight from the sim's position buffer — edges as lines, nodes as instanced
circle quads — plus an integer "pick" pass (node index → texture). It's how
`verify-gpu` draws to an offscreen texture and asserts pixels + a pick land, and
it's the basis for a future zero-round-trip WebGPU render path (today the app
renders the GPU-computed positions through R3F/WebGL).

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
