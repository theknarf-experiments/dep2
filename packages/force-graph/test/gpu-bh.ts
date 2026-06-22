// Barnes-Hut verification + scale benchmark. The exact all-pairs charge is
// already verified bit-exact against d3 (gpu-oracle.ts), so here we check the
// Barnes-Hut charge reproduces the exact one — per-step forces within Barnes-Hut
// tolerance, and the converged layout matching — then benchmark to millions.
//
//   mise run verify-gpu-bh

import { GpuLayout } from "../src/gpu/sim.ts";

function seeded(seed: number) {
  let s = seed >>> 0;
  return () => { s ^= s << 13; s ^= s >>> 17; s ^= s << 5; return (s >>> 0) / 0xffffffff; };
}
function randomGraph(n: number, avgDeg: number, rnd: () => number): Uint32Array {
  const m = Math.floor((n * avgDeg) / 2);
  const e = new Uint32Array(m * 2);
  for (let i = 1; i < n; i++) { e[(i - 1) * 2] = i; e[(i - 1) * 2 + 1] = (rnd() * i) | 0; }
  for (let i = n - 1; i < m; i++) { e[i * 2] = (rnd() * n) | 0; e[i * 2 + 1] = (rnd() * n) | 0; }
  return e;
}
function seedPositions(n: number, rnd: () => number): Float32Array {
  const half = Math.max(60, 38 * Math.sqrt(n) * 0.5);
  const a = new Float32Array(n * 2);
  for (let i = 0; i < n * 2; i++) a[i] = (rnd() * 2 - 1) * half;
  return a;
}
async function run(device: GPUDevice, n: number, edges: Uint32Array, seed: Float32Array, mode: "exact" | "bh", steps: number) {
  const sim = new GpuLayout({ device, nodeCount: n, edges, positions: seed.slice(), chargeMode: mode });
  for (let i = 0; i < steps; i++) sim.step();
  const p = await sim.readPositions();
  sim.destroy();
  return p;
}
function spearman(a: number[], b: number[]) {
  const rank = (xs: number[]) => { const idx = xs.map((_, i) => i).sort((i, j) => xs[i] - xs[j]); const r = new Array(xs.length); idx.forEach((id, k) => (r[id] = k)); return r; };
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
function edgeMean(p: Float32Array, edges: Uint32Array) {
  const m = edges.length / 2; let s = 0;
  for (let i = 0; i < m; i++) s += Math.hypot(p[2 * edges[2 * i]] - p[2 * edges[2 * i + 1]], p[2 * edges[2 * i] + 1] - p[2 * edges[2 * i + 1] + 1]);
  return s / m;
}

const adapter = await navigator.gpu?.requestAdapter();
if (!adapter) { console.error("no WebGPU adapter"); Deno.exit(1); }
const device = await adapter.requestDevice();
let failed = false;
const check = (ok: boolean, msg: string) => { console.log(`  ${ok ? "ok  " : "FAIL"}  ${msg}`); if (!ok) failed = true; };

// ---- 1. one-step charge force: Barnes-Hut vs exact (same seed) ----
console.log("Barnes-Hut vs exact charge:\n");
for (const n of [2000, 8000]) {
  const edges = randomGraph(n, 4, seeded(3));
  const seed = seedPositions(n, seeded(42));
  const ex1 = await run(device, n, edges, seed, "exact", 1);
  const bh1 = await run(device, n, edges, seed, "bh", 1);
  // per-node displacement difference relative to the exact displacement
  let es = 0, ds = 0;
  for (let i = 0; i < n; i++) {
    const dxe = ex1[2 * i] - seed[2 * i], dye = ex1[2 * i + 1] - seed[2 * i + 1];
    const exx = bh1[2 * i] - ex1[2 * i], eyy = bh1[2 * i + 1] - ex1[2 * i + 1];
    ds += dxe * dxe + dye * dye; es += exx * exx + eyy * eyy;
  }
  const rel = Math.sqrt(es / ds);
  check(rel < 0.2, `n=${n}: 1-step force within Barnes-Hut tolerance of exact (rel ${rel.toFixed(3)})`);
}

// ---- 2. converged layout: Barnes-Hut vs exact ----
console.log("\nConverged layout (400 steps):\n");
for (const n of [2000, 8000]) {
  const edges = randomGraph(n, 4, seeded(3));
  const seed = seedPositions(n, seeded(42));
  const ex = await run(device, n, edges, seed, "exact", 400);
  const bh = await run(device, n, edges, seed, "bh", 400);
  const sp = pairwiseSpearman(ex, bh, n);
  const me = edgeMean(ex, edges), mb = edgeMean(bh, edges);
  let nan = 0; for (let i = 0; i < n * 2; i++) if (!Number.isFinite(bh[i])) nan++;
  console.log(`  n=${n}: edge len exact ${me.toFixed(1)}  bh ${mb.toFixed(1)}  pairwise-Spearman ${sp.toFixed(3)}  nan ${nan}`);
  check(nan === 0, `n=${n}: Barnes-Hut produced no NaN`);
  check(sp > 0.9, `n=${n}: Barnes-Hut layout matches exact (Spearman > 0.9)`);
  check(Math.abs(mb - me) / me < 0.15, `n=${n}: edge length within 15% of exact`);
}

// ---- 3. scale benchmark ----
console.log("\nscale (Barnes-Hut, ms/step, mean of 10 after warmup):");
for (const n of [100000, 1000000, 2000000, 4000000]) {
  device.pushErrorScope("out-of-memory");
  device.pushErrorScope("validation");
  const edges = randomGraph(n, 4, seeded(7));
  const sim = new GpuLayout({ device, nodeCount: n, edges, chargeMode: "bh" });
  for (let i = 0; i < 3; i++) sim.step();
  const warm = await sim.readPositions();
  const t0 = performance.now();
  const T = 10;
  sim.step(T);
  await sim.readPositions();
  const ms = (performance.now() - t0) / T;
  const valErr = await device.popErrorScope();
  const oomErr = await device.popErrorScope();
  const moved = Number.isFinite(warm[0]) && (warm[0] !== 0 || warm[1] !== 0);
  const err = oomErr ?? valErr;
  if (err) console.log(`  n=${String(n).padStart(8)}  SKIPPED (${err.message.split("\n")[0]})`);
  else if (!moved) console.log(`  n=${String(n).padStart(8)}  SKIPPED (no movement — likely a device limit)`);
  else console.log(`  n=${String(n).padStart(8)} (L=${sim.maxLevel}, grid ${sim.gridRes})  ${ms.toFixed(1)} ms/step  (~${(1000 / ms).toFixed(0)} steps/s)`);
  sim.destroy();
}

if (failed) { console.error("\nBARNES-HUT FAILED"); Deno.exit(1); }
console.log("\nBARNES-HUT OK");
