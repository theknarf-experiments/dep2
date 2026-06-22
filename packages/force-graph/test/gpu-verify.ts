// Headless WebGPU verification of GpuLayout (run with Deno + Metal/wgpu):
//   deno run --unstable-webgpu --allow-read sim verify.ts
// Checks the layout converges sensibly on a known graph and benchmarks scale.

import { GpuLayout } from "../src/gpu/sim.ts";
import { GpuRenderer } from "../src/gpu/render.ts";

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

// ---- disconnected components push apart (the whole point) ----
{
  const k = 20, cs = k * k, n = cs * 2; // two 20x20 meshes, no edges between them
  const e: number[] = [];
  const meshEdges = (off: number) => {
    for (let y = 0; y < k; y++) for (let x = 0; x < k; x++) {
      const i = off + y * k + x;
      if (x + 1 < k) e.push(i, i + 1);
      if (y + 1 < k) e.push(i, i + k);
    }
  };
  meshEdges(0); meshEdges(cs);
  const edges = new Uint32Array(e);
  // seed the two clusters slightly offset and overlapping, then see if repulsion
  // drives them apart into separated balls
  const seed = new Float32Array(n * 2);
  let s = 7;
  const r = () => ((((s = (s * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff) * 2 - 1) * 80);
  for (let i = 0; i < cs; i++) { seed[i * 2] = -80 + r(); seed[i * 2 + 1] = r(); }
  for (let i = cs; i < n; i++) { seed[i * 2] = 80 + r(); seed[i * 2 + 1] = r(); }
  const sim = new GpuLayout({ device, nodeCount: n, edges, positions: seed });
  const cen = (pos: Float32Array, off: number) => {
    let sx = 0, sy = 0;
    for (let i = 0; i < cs; i++) { sx += pos[(off + i) * 2]; sy += pos[(off + i) * 2 + 1]; }
    return [sx / cs, sy / cs] as [number, number];
  };
  const dist = (a: [number, number], b: [number, number]) => Math.hypot(a[0] - b[0], a[1] - b[1]);
  const p0 = await sim.readPositions();
  const gap0 = dist(cen(p0, 0), cen(p0, cs));
  for (let i = 0; i < 500; i++) sim.step();
  const p1 = await sim.readPositions();
  const cA = cen(p1, 0), cB = cen(p1, cs);
  const gap1 = dist(cA, cB);
  let radA = 0;
  for (let i = 0; i < cs; i++) radA += dist([p1[i * 2], p1[i * 2 + 1]], cA);
  radA /= cs;
  console.log(`\ndisconnected clusters: centroid gap ${gap0.toFixed(0)} -> ${gap1.toFixed(0)}, cluster radius ${radA.toFixed(0)}`);
  check(gap1 > gap0, "disconnected components moved further apart (repel, not attract)");
  check(gap1 > radA * 2, "components separated (gap > cluster size), not piled together");
  sim.destroy();
}

// ---- warm restart: a settled layout survives a rebuild (positions + low alpha) ----
{
  const { n, edges } = gridGraph(30); // 900
  const sim = new GpuLayout({ device, nodeCount: n, edges });
  for (let i = 0; i < 300; i++) sim.step();
  const p1 = await sim.readPositions();
  sim.destroy();
  // rebuild with the same positions + one new node, low alpha (a stream update)
  const seed = new Float32Array((n + 1) * 2);
  seed.set(p1);
  seed[n * 2] = p1[0] + 5; seed[n * 2 + 1] = p1[1] + 5;
  const warm = new GpuLayout({ device, nodeCount: n + 1, edges, positions: seed, alpha: 0.05 });
  for (let i = 0; i < 10; i++) warm.step();
  const p2 = await warm.readPositions();
  let drift = 0;
  for (let i = 0; i < n; i++) drift += Math.hypot(p1[2 * i] - p2[2 * i], p1[2 * i + 1] - p2[2 * i + 1]);
  drift /= n;
  // contrast: a cold start (random seed, alpha 1) moves everything a lot
  const cold = new GpuLayout({ device, nodeCount: n, edges });
  const a = await cold.readPositions();
  for (let i = 0; i < 10; i++) cold.step();
  const b = await cold.readPositions();
  let coldDrift = 0;
  for (let i = 0; i < n; i++) coldDrift += Math.hypot(a[2 * i] - b[2 * i], a[2 * i + 1] - b[2 * i + 1]);
  coldDrift /= n;
  console.log(`\nwarm restart: existing-node drift ${drift.toFixed(2)} vs cold start ${coldDrift.toFixed(2)} (10 steps)`);
  check(drift < 8, "warm restart keeps settled positions (small drift)");
  check(coldDrift > drift * 3, "warm restart drifts far less than a cold start");
  warm.destroy();
  cold.destroy();
}

// ---- renderer: draws to an offscreen texture and picks the node under a pixel ----
{
  const { n, edges } = gridGraph(40); // 1600
  const sim = new GpuLayout({ device, nodeCount: n, edges });
  for (let i = 0; i < 200; i++) sim.step();
  const pos = await sim.readPositions();
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (let i = 0; i < n; i++) {
    minX = Math.min(minX, pos[2 * i]); maxX = Math.max(maxX, pos[2 * i]);
    minY = Math.min(minY, pos[2 * i + 1]); maxY = Math.max(maxY, pos[2 * i + 1]);
  }
  const cam = { zoom: Math.min(256, 256) / (Math.max(maxX - minX, maxY - minY) * 1.2), cx: (minX + maxX) / 2, cy: (minY + maxY) / 2 };
  const hi = { hovered: -1, selected: -1, activeGroup: -1 };
  const W = 256, H = 256;
  const fmt: GPUTextureFormat = "rgba8unorm";
  const tex = device.createTexture({ size: { width: W, height: H }, format: fmt, usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC });
  const r = new GpuRenderer(device, fmt);
  r.setGraph({
    n, posBuffer: sim.positions, edgeBuffer: sim.edgeBuffer, edgeCount: edges.length >>> 1,
    colors: new Uint32Array(n).fill(0xff66cc33), // packed 0xAABBGGRR
    radii: new Float32Array(n).fill(6),
    groups: new Uint32Array(n),
  });
  r.draw(tex.createView(), W, H, cam, hi);
  // read pixels back, count non-background
  const bpr = W * 4; // 1024, 256-aligned
  const stg = device.createBuffer({ size: bpr * H, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
  const enc = device.createCommandEncoder();
  enc.copyTextureToBuffer({ texture: tex }, { buffer: stg, bytesPerRow: bpr }, { width: W, height: H });
  device.queue.submit([enc.finish()]);
  await stg.mapAsync(GPUMapMode.READ);
  const px = new Uint8Array(stg.getMappedRange());
  let nonBg = 0;
  for (let i = 0; i < W * H; i++) {
    if (Math.abs(px[i * 4] - 14) > 6 || Math.abs(px[i * 4 + 1] - 14) > 6 || Math.abs(px[i * 4 + 2] - 17) > 6) nonBg++;
  }
  stg.unmap();
  console.log(`\nrender 40x40 -> ${W}x${H}: non-background pixels = ${nonBg}`);
  check(nonBg > 200, "renderer drew nodes/edges to the texture");
  // pick at node 0's pixel
  const clipX = (pos[0] - cam.cx) * cam.zoom / (W / 2);
  const clipY = (pos[1] - cam.cy) * cam.zoom / (H / 2);
  const pxX = (clipX * 0.5 + 0.5) * W;
  const pxY = (0.5 - clipY * 0.5) * H;
  const idx = await r.pick(pxX, pxY, W, H, cam, hi);
  console.log(`  pick at node0 pixel (${pxX.toFixed(0)},${pxY.toFixed(0)}) -> node ${idx}`);
  check(idx >= 0 && idx < n, "GPU pick returns a node at a node's pixel");
  r.destroy();
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
