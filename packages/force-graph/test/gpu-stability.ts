// Stability oracle: the GPU sim must not blow up where d3-force stays bounded.
// d3 never "yeets" nodes to infinity on normal graphs; if ours does, that's a
// bug in our port (not the algorithm). We run d3 (theta 0, matching our exact
// charge) and the GPU sim on graphs designed to stress the layout — hubs, dense
// graphs, complete graphs, and a streaming-like seed where some nodes start very
// far away — and require the GPU to stay finite and within a sane multiple of
// d3's extent.
//
//   mise run verify-gpu-stability

import { forceSimulation, forceManyBody, forceLink, forceX, forceY } from "npm:d3-force@3";
import { GpuLayout, DEFAULT_PARAMS as P } from "../src/gpu/sim.ts";

const VELOCITY_DECAY = 1 - P.velDecay;
const ALPHA_DECAY = 1 - Math.pow(0.001, 1 / 300);
const ALPHA_MIN = 0.001;
const STEPS = 400;

function seeded(seed: number) {
  let s = seed >>> 0;
  return () => { s ^= s << 13; s ^= s >>> 17; s ^= s << 5; return (s >>> 0) / 0xffffffff; };
}

// ---- adversarial graphs ----
function denseRandom(n: number, avgDeg: number, rnd: () => number): Uint32Array {
  const e: number[] = [];
  for (let i = 1; i < n; i++) e.push(i, (rnd() * i) | 0); // spanning tree
  const extra = Math.floor((n * avgDeg) / 2);
  for (let k = 0; k < extra; k++) {
    const a = (rnd() * n) | 0, b = (rnd() * n) | 0;
    if (a !== b) e.push(a, b);
  }
  return new Uint32Array(e);
}
function scaleFree(n: number, m: number, rnd: () => number): Uint32Array {
  // Barabasi-Albert-ish preferential attachment -> hubs.
  const e: number[] = [];
  const targets: number[] = [0, 1];
  e.push(0, 1);
  for (let v = 2; v < n; v++) {
    const picked = new Set<number>();
    for (let j = 0; j < Math.min(m, targets.length); j++) {
      picked.add(targets[(rnd() * targets.length) | 0]);
    }
    for (const t of picked) { e.push(v, t); targets.push(v, t); }
  }
  return new Uint32Array(e);
}
function complete(n: number): Uint32Array {
  const e: number[] = [];
  for (let i = 0; i < n; i++) for (let j = i + 1; j < n; j++) e.push(i, j);
  return new Uint32Array(e);
}
function star(leaves: number): Uint32Array {
  const e: number[] = [];
  for (let i = 1; i <= leaves; i++) e.push(0, i);
  return new Uint32Array(e);
}

function d3Run(n: number, edges: Uint32Array, seed: Float32Array): Float32Array {
  const nodes = Array.from({ length: n }, (_, i) => ({ index: i, x: seed[2 * i], y: seed[2 * i + 1], vx: 0, vy: 0 }));
  const links: any[] = [];
  for (let i = 0; i < edges.length; i += 2) links.push({ source: edges[i], target: edges[i + 1] });
  const sim = forceSimulation(nodes as any)
    .randomSource(seeded(1)).alphaDecay(ALPHA_DECAY).alphaMin(ALPHA_MIN).velocityDecay(VELOCITY_DECAY)
    .force("charge", forceManyBody().strength(P.charge).distanceMin(Math.sqrt(P.distanceMin2)).theta(0))
    // d3 default link strength is 1/min(deg) — matching the GPU; don't override it.
    .force("link", forceLink(links).id((d: any) => d.index).distance(P.linkDistance))
    .force("x", forceX(0).strength(P.center)).force("y", forceY(0).strength(P.center)).stop();
  for (let i = 0; i < STEPS; i++) sim.tick();
  const o = new Float32Array(n * 2);
  nodes.forEach((nd, i) => { o[2 * i] = nd.x; o[2 * i + 1] = nd.y; });
  return o;
}
async function gpuRun(device: GPUDevice, n: number, edges: Uint32Array, seed: Float32Array): Promise<Float32Array> {
  const sim = new GpuLayout({ device, nodeCount: n, edges, positions: seed.slice(), alphaDecay: ALPHA_DECAY, alphaMin: ALPHA_MIN });
  for (let i = 0; i < STEPS; i++) sim.step();
  return sim.readPositions().finally(() => sim.destroy());
}
function extent(p: Float32Array) {
  let maxAbs = 0, nan = 0;
  for (let i = 0; i < p.length; i++) {
    if (!Number.isFinite(p[i])) { nan++; continue; }
    maxAbs = Math.max(maxAbs, Math.abs(p[i]));
  }
  return { maxAbs, nan };
}

const adapter = await navigator.gpu?.requestAdapter();
if (!adapter) { console.error("no WebGPU adapter"); Deno.exit(1); }
const device = await adapter.requestDevice();
let failed = false;
const check = (ok: boolean, msg: string) => { console.log(`  ${ok ? "ok  " : "FAIL"}  ${msg}`); if (!ok) failed = true; };

interface Case { name: string; n: number; edges: Uint32Array; spread: number }
const r = seeded(42);
const cases: Case[] = [
  { name: "dense random (n=800, deg~12)", n: 800, edges: denseRandom(800, 12, seeded(3)), spread: 600 },
  { name: "scale-free hubs (n=1000)", n: 1000, edges: scaleFree(1000, 3, seeded(4)), spread: 700 },
  { name: "complete K40", n: 40, edges: complete(40), spread: 400 },
  { name: "star (200 leaves)", n: 201, edges: star(200), spread: 500 },
  // streaming-like: most nodes near a settled cluster, a few flung very far, all linked in.
  { name: "streaming mismatch (far outliers)", n: 600, edges: denseRandom(600, 4, seeded(6)), spread: 400 },
];

console.log(`GPU stability vs d3 — ${STEPS} steps\n`);
for (const c of cases) {
  const seed = new Float32Array(c.n * 2);
  for (let i = 0; i < c.n * 2; i++) seed[i] = (r() * 2 - 1) * c.spread;
  if (c.name.startsWith("streaming")) {
    // fling 5% of nodes 30x further out (simulates new nodes linked to far ones)
    for (let i = 0; i < c.n; i++) if (r() < 0.05) { seed[2 * i] *= 30; seed[2 * i + 1] *= 30; }
  }
  const d = d3Run(c.n, c.edges, seed);
  const g = await gpuRun(device, c.n, c.edges, seed);
  const ed = extent(d), eg = extent(g);
  console.log(`${c.name} (n=${c.n}, m=${c.edges.length / 2}):`);
  console.log(`    d3  maxAbs ${ed.maxAbs.toFixed(0)} nan ${ed.nan}   gpu maxAbs ${eg.maxAbs.toFixed(0)} nan ${eg.nan}`);
  check(eg.nan === 0, "GPU produced no NaN/Inf");
  check(eg.maxAbs < Math.max(5000, ed.maxAbs * 5), "GPU stayed bounded (within 5x of d3's extent)");
  console.log();
}

if (failed) { console.error("STABILITY FAILED — GPU blows up where d3 stays bounded"); Deno.exit(1); }
console.log("STABILITY OK");
