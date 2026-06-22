// Headless WebGPU verification of GpuLayout (run with Deno + Metal/wgpu):
//   deno run --unstable-webgpu --allow-read sim verify.ts
// Checks the layout converges sensibly on a known graph and benchmarks scale.

import { GpuLayout } from "../src/gpu/sim.ts";

function gridGraph(k: number): { n: number; edges: Uint32Array } {
  const n = k * k;
  const e: number[] = [];
  for (let y = 0; y < k; y++) {
    for (let x = 0; x < k; x++) {
      const i = y * k + x;
      if (x + 1 < k) e.push(i, i + 1);
      if (y + 1 < k) e.push(i, i + k);
    }
  }
  return { n, edges: new Uint32Array(e) };
}

function randomGraph(n: number, avgDeg: number): { n: number; edges: Uint32Array } {
  const m = Math.floor((n * avgDeg) / 2);
  const e = new Uint32Array(m * 2);
  let s = 12345;
  const rnd = () => ((s = (s * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff);
  for (let i = 1; i < n; i++) {
    e[(i - 1) * 2] = i;
    e[(i - 1) * 2 + 1] = (rnd() * i) | 0;
  } // spanning tree (connected)
  for (let i = n - 1; i < m; i++) {
    e[i * 2] = (rnd() * n) | 0;
    e[i * 2 + 1] = (rnd() * n) | 0;
  }
  return { n, edges: e };
}

function stats(pos: Float32Array, edges: Uint32Array, n: number) {
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity, nan = 0;
  for (let i = 0; i < n; i++) {
    const x = pos[2 * i], y = pos[2 * i + 1];
    if (!Number.isFinite(x) || !Number.isFinite(y)) { nan++; continue; }
    minX = Math.min(minX, x); maxX = Math.max(maxX, x);
    minY = Math.min(minY, y); maxY = Math.max(maxY, y);
  }
  let edgeLen = 0;
  const m = edges.length >>> 1;
  for (let i = 0; i < m; i++) {
    const a = edges[2 * i], b = edges[2 * i + 1];
    edgeLen += Math.hypot(pos[2 * a] - pos[2 * b], pos[2 * a + 1] - pos[2 * b + 1]);
  }
  // average distance between random (likely unconnected) pairs
  let randLen = 0, samples = 2000;
  let s = 999;
  const rnd = () => ((s = (s * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff);
  for (let i = 0; i < samples; i++) {
    const a = (rnd() * n) | 0, b = (rnd() * n) | 0;
    randLen += Math.hypot(pos[2 * a] - pos[2 * b], pos[2 * a + 1] - pos[2 * b + 1]);
  }
  return {
    nan,
    bbox: [maxX - minX, maxY - minY] as [number, number],
    avgEdge: edgeLen / m,
    avgRand: randLen / samples,
  };
}

const adapter = await navigator.gpu?.requestAdapter();
if (!adapter) { console.error("no WebGPU adapter"); Deno.exit(1); }
const device = await adapter.requestDevice();
let failed = false;
const check = (cond: boolean, msg: string) => {
  console.log(`${cond ? "ok  " : "FAIL"}  ${msg}`);
  if (!cond) failed = true;
};

// ---- correctness: a grid mesh should cluster connected nodes ----
{
  const { n, edges } = gridGraph(80); // 6400 nodes
  const sim = new GpuLayout({ device, nodeCount: n, edges });
  for (let i = 0; i < 400; i++) sim.step();
  const a = stats(await sim.readPositions(), edges, n);
  console.log(`\ngrid 80x80 (n=${n}): nan=${a.nan} bbox=${a.bbox.map((v) => v.toFixed(0))} avgEdge=${a.avgEdge.toFixed(1)} avgRand=${a.avgRand.toFixed(1)}`);
  check(a.nan === 0, "no NaN/Inf positions");
  check(a.bbox[0] > 1 && a.bbox[1] > 1, "layout has spread (not collapsed)");
  check(a.avgEdge < a.avgRand * 0.5, "connected nodes closer than random pairs (structure formed)");
  check(a.avgEdge > 2 && a.avgEdge < 400, "edge length in a sane band");
  // cooling: little movement after settling
  const before = await sim.readPositions();
  for (let i = 0; i < 5; i++) sim.step();
  const after = await sim.readPositions();
  let disp = 0;
  for (let i = 0; i < n; i++) disp += Math.hypot(before[2 * i] - after[2 * i], before[2 * i + 1] - after[2 * i + 1]);
  disp /= n;
  console.log(`  mean displacement / 5 steps after settling: ${disp.toFixed(3)}`);
  check(disp < a.avgEdge, "settled (movement small vs edge length)");
  sim.destroy();
}

// ---- scale: ms/step at size ----
console.log("\nscale (ms/step, mean of 20 after warmup):");
for (const n of [10000, 100000, 1000000]) {
  const { edges } = randomGraph(n, 4);
  const sim = new GpuLayout({ device, nodeCount: n, edges });
  for (let i = 0; i < 3; i++) sim.step();
  await sim.readPositions(); // sync GPU
  const t0 = performance.now();
  const T = 20;
  sim.step(T);
  await sim.readPositions(); // sync
  const ms = (performance.now() - t0) / T;
  console.log(`  n=${String(n).padStart(8)}  ${ms.toFixed(1)} ms/step  (~${(1000 / ms).toFixed(0)} steps/s)`);
  sim.destroy();
}

if (failed) { console.error("\nVERIFY FAILED"); Deno.exit(1); }
console.log("\nVERIFY OK");
