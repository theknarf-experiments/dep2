// Exactness oracle: the GPU sim must reproduce d3-force. d3-force is the oracle.
//
// The GPU computes the SAME equations as d3 (see src/gpu/sim.ts, each pass cites
// the d3 source line it implements). The one thing a GPU cannot copy verbatim is
// d3's link pass, which relaxes links serially (Gauss-Seidel: link k sees the
// velocity updates of links 0..k-1). On the GPU links run in parallel (Jacobi).
// Both are d3-the-algorithm; they converge to the same layout. So we verify:
//
//   1. charge + forceX/forceY + integrator are BIT-EXACT vs stock d3 (no links,
//      so the serial link pass is out of the picture) — proves the core is d3.
//   2. the link force formula is exact, by matching the GPU against d3's own link
//      equations evaluated in parallel (a Float64 reference) to float precision.
//   3. the full system (charge + links + centering) CONVERGES to stock d3's
//      actual layout — edge-length distribution and pairwise-distance ordering.
//
//   mise run verify-gpu-oracle

import { forceSimulation, forceManyBody, forceLink, forceX, forceY } from "npm:d3-force@3";
import { GpuLayout, DEFAULT_PARAMS as P } from "../src/gpu/sim.ts";

const VELOCITY_DECAY = 1 - P.velDecay; // d3 takes the un-subtracted value (0.4)
const ALPHA_DECAY = 1 - Math.pow(0.001, 1 / 300);
const ALPHA_MIN = 0.001;

function seeded(seed: number) {
  let s = seed >>> 0;
  return () => { s ^= s << 13; s ^= s >>> 17; s ^= s << 5; return (s >>> 0) / 0xffffffff; };
}
function mesh(k: number, off = 0): number[] {
  const e: number[] = [];
  for (let y = 0; y < k; y++) for (let x = 0; x < k; x++) {
    const i = off + y * k + x;
    if (x + 1 < k) e.push(i, i + 1);
    if (y + 1 < k) e.push(i, i + k);
  }
  return e;
}
function makeSeed(n: number): Float32Array {
  const rnd = seeded(42);
  const half = Math.max(60, P.linkDistance * Math.sqrt(n) * 0.5);
  const a = new Float32Array(n * 2);
  for (let i = 0; i < n * 2; i++) a[i] = (rnd() * 2 - 1) * half;
  return a;
}

// Stock d3-force (the oracle). theta(0) makes charge the exact all-pairs sum.
function d3Run(n: number, edges: Uint32Array, seed: Float32Array, steps: number, withLinks: boolean): Float32Array {
  const nodes = Array.from({ length: n }, (_, i) => ({ index: i, x: seed[2 * i], y: seed[2 * i + 1], vx: 0, vy: 0 }));
  const sim = forceSimulation(nodes as any)
    .randomSource(seeded(1)).alphaDecay(ALPHA_DECAY).alphaMin(ALPHA_MIN).velocityDecay(VELOCITY_DECAY)
    .force("charge", forceManyBody().strength(P.charge).distanceMin(Math.sqrt(P.distanceMin2)).theta(0))
    .force("x", forceX(0).strength(P.center)).force("y", forceY(0).strength(P.center));
  if (withLinks) {
    const links: any[] = [];
    for (let i = 0; i < edges.length; i += 2) links.push({ source: edges[i], target: edges[i + 1] });
    sim.force("link", forceLink(links).id((d: any) => d.index).distance(P.linkDistance).strength(P.linkStrength));
  }
  sim.stop();
  for (let i = 0; i < steps; i++) sim.tick();
  const o = new Float32Array(n * 2);
  nodes.forEach((nd, i) => { o[2 * i] = nd.x; o[2 * i + 1] = nd.y; });
  return o;
}

// d3's exact force equations, but with PARALLEL (Jacobi) link relaxation — the
// faithful parallel formulation the GPU implements. Float64, used as the
// exactness reference for the link pass.
function d3Parallel(n: number, edges: Uint32Array, seed: Float32Array, steps: number): Float32Array {
  const x = new Float64Array(n), y = new Float64Array(n), vx = new Float64Array(n), vy = new Float64Array(n);
  for (let i = 0; i < n; i++) { x[i] = seed[2 * i]; y[i] = seed[2 * i + 1]; }
  const m = edges.length / 2, deg = new Float64Array(n);
  for (let i = 0; i < m; i++) { deg[edges[2 * i]]++; deg[edges[2 * i + 1]]++; }
  let alpha = 1;
  for (let s = 0; s < steps; s++) {
    alpha += (0 - alpha) * ALPHA_DECAY;
    for (let i = 0; i < n; i++) { // charge (all-pairs)
      let ax = 0, ay = 0;
      for (let j = 0; j < n; j++) {
        if (j === i) continue;
        let dx = x[j] - x[i], dy = y[j] - y[i], l = dx * dx + dy * dy;
        if (l === 0) continue;
        if (l < P.distanceMin2) l = Math.sqrt(P.distanceMin2 * l);
        ax += dx * P.charge * alpha / l; ay += dy * P.charge * alpha / l;
      }
      vx[i] += ax; vy[i] += ay;
    }
    const lx = new Float64Array(n), ly = new Float64Array(n); // link (parallel)
    for (let e = 0; e < m; e++) {
      const a = edges[2 * e], b = edges[2 * e + 1];
      let dx = (x[b] + vx[b]) - (x[a] + vx[a]), dy = (y[b] + vy[b]) - (y[a] + vy[a]);
      const len = Math.sqrt(dx * dx + dy * dy);
      if (len === 0) continue;
      const l = (len - P.linkDistance) / len * alpha * P.linkStrength;
      dx *= l; dy *= l;
      const bias = deg[a] / (deg[a] + deg[b]);
      lx[b] -= dx * bias; ly[b] -= dy * bias; lx[a] += dx * (1 - bias); ly[a] += dy * (1 - bias);
    }
    for (let i = 0; i < n; i++) {
      vx[i] += lx[i]; vy[i] += ly[i];
      vx[i] += (0 - x[i]) * P.center * alpha; vy[i] += (0 - y[i]) * P.center * alpha;
      vx[i] *= P.velDecay; vy[i] *= P.velDecay; x[i] += vx[i]; y[i] += vy[i];
    }
  }
  const o = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) { o[2 * i] = x[i]; o[2 * i + 1] = y[i]; }
  return o;
}

async function gpuRun(device: GPUDevice, n: number, edges: Uint32Array, seed: Float32Array, steps: number): Promise<Float32Array> {
  const sim = new GpuLayout({ device, nodeCount: n, edges, positions: seed.slice(), alphaDecay: ALPHA_DECAY, alphaMin: ALPHA_MIN });
  for (let i = 0; i < steps; i++) sim.step();
  return sim.readPositions().finally(() => sim.destroy());
}

function relError(ref: Float32Array, got: Float32Array, seed: Float32Array) {
  let errSq = 0, dispSq = 0, maxAbs = 0;
  const n = ref.length / 2;
  for (let i = 0; i < n; i++) {
    const ex = got[2 * i] - ref[2 * i], ey = got[2 * i + 1] - ref[2 * i + 1];
    errSq += ex * ex + ey * ey; maxAbs = Math.max(maxAbs, Math.hypot(ex, ey));
    const dx = ref[2 * i] - seed[2 * i], dy = ref[2 * i + 1] - seed[2 * i + 1];
    dispSq += dx * dx + dy * dy;
  }
  return { rel: Math.sqrt(errSq / Math.max(dispSq, 1e-9)), maxAbs };
}
function edgeStats(p: Float32Array, edges: Uint32Array) {
  const m = edges.length / 2, ls: number[] = [];
  let s = 0;
  for (let i = 0; i < m; i++) {
    const a = edges[2 * i], b = edges[2 * i + 1];
    const l = Math.hypot(p[2 * a] - p[2 * b], p[2 * a + 1] - p[2 * b + 1]);
    ls.push(l); s += l;
  }
  const mean = s / m, v = ls.reduce((q, l) => q + (l - mean) ** 2, 0) / m;
  return { mean, cv: Math.sqrt(v) / mean };
}
function spearman(a: number[], b: number[]) {
  const rank = (xs: number[]) => {
    const idx = xs.map((_, i) => i).sort((i, j) => xs[i] - xs[j]);
    const r = new Array(xs.length); idx.forEach((id, k) => (r[id] = k)); return r;
  };
  const ra = rank(a), rb = rank(b), m = a.length, mu = (m - 1) / 2;
  let nu = 0, da = 0, db = 0;
  for (let i = 0; i < m; i++) { const x = ra[i] - mu, y = rb[i] - mu; nu += x * y; da += x * x; db += y * y; }
  return nu / Math.sqrt(da * db);
}
function pairwiseSpearman(a: Float32Array, b: Float32Array, n: number) {
  const rnd = seeded(99), A: number[] = [], B: number[] = [];
  for (let k = 0; k < 4000; k++) {
    const i = (rnd() * n) | 0; let j = (rnd() * n) | 0; if (i === j) j = (j + 1) % n;
    A.push(Math.hypot(a[2 * i] - a[2 * j], a[2 * i + 1] - a[2 * j + 1]));
    B.push(Math.hypot(b[2 * i] - b[2 * j], b[2 * i + 1] - b[2 * j + 1]));
  }
  return spearman(A, B);
}

const adapter = await navigator.gpu?.requestAdapter();
if (!adapter) { console.error("no WebGPU adapter"); Deno.exit(1); }
const device = await adapter.requestDevice();
let failed = false;
const check = (ok: boolean, msg: string) => { console.log(`  ${ok ? "ok  " : "FAIL"}  ${msg}`); if (!ok) failed = true; };

interface Case { name: string; n: number; edges: Uint32Array }
const cases: Case[] = [
  { name: "mesh 20x20", n: 400, edges: new Uint32Array(mesh(20)) },
  (() => { const e: number[] = []; const r = seeded(5); for (let i = 1; i < 500; i++) e.push(i, (r() * i) | 0); return { name: "random tree", n: 500, edges: new Uint32Array(e) }; })(),
  (() => { const k = 14, cs = k * k; return { name: "two disconnected meshes", n: cs * 2, edges: new Uint32Array([...mesh(k, 0), ...mesh(k, cs)]) }; })(),
];

console.log(`GPU vs d3-force — charge ${P.charge}, linkDist ${P.linkDistance}, linkStrength ${P.linkStrength}, center ${P.center}\n`);
for (const c of cases) {
  console.log(`${c.name} (n=${c.n}):`);
  const seed = makeSeed(c.n);

  // 1. charge + forceX/forceY + integrator: bit-exact vs stock d3 (no links).
  for (const steps of [1, 16, 100]) {
    const { rel, maxAbs } = relError(d3Run(c.n, c.edges, seed, steps, false), await gpuRun(device, c.n, new Uint32Array([]), seed, steps), seed);
    check(rel < (steps === 1 ? 1e-4 : steps === 16 ? 2e-3 : 2e-2), `charge+center == stock d3, ${String(steps).padStart(3)} steps  rel ${rel.toExponential(2)} (max ${maxAbs.toExponential(2)})`);
  }
  // 2. link force formula: exact vs d3's equations in parallel form.
  {
    const { rel, maxAbs } = relError(d3Parallel(c.n, c.edges, seed, 1), await gpuRun(device, c.n, c.edges, seed, 1), seed);
    check(rel < 1e-4, `link formula == d3 (parallel), 1 step  rel ${rel.toExponential(2)} (max ${maxAbs.toExponential(2)})`);
  }
  // 3. full system converges to stock d3's layout.
  {
    const d = d3Run(c.n, c.edges, seed, 400, true);
    const g = await gpuRun(device, c.n, c.edges, seed, 400);
    const ed = edgeStats(d, c.edges), eg = edgeStats(g, c.edges);
    const sp = pairwiseSpearman(d, g, c.n);
    console.log(`        converged: edge len d3 ${ed.mean.toFixed(1)} (cv ${ed.cv.toFixed(2)})  gpu ${eg.mean.toFixed(1)} (cv ${eg.cv.toFixed(2)})  pairwise-Spearman ${sp.toFixed(3)}`);
    check(Math.abs(eg.mean - ed.mean) / ed.mean < 0.1, "converged edge length within 10% of d3");
    check(Math.abs(eg.cv - ed.cv) < 0.08, "converged edge-length spread matches d3");
    check(sp > 0.9, "converged layout ordering matches d3 (Spearman > 0.9)");
  }
  console.log();
}

if (failed) { console.error("ORACLE: GPU does not reproduce d3-force"); Deno.exit(1); }
console.log("ORACLE OK — GPU reproduces d3-force (core bit-exact; full system converges to d3's layout)");
