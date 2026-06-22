/// <reference types="@webgpu/types" />
// GPU force-directed layout (WebGPU compute). Pure WebGPU — no DOM — so it runs
// in the browser and in Deno (for headless verification). Positions live in a GPU
// buffer the whole time; a renderer binds it directly (no CPU round-trip), which
// is what makes millions of nodes viable.
//
// The point of the layout is to PUSH THINGS APART so structure is legible: nodes
// don't overlap and disconnected components separate. Repulsion is genuine
// n-body, approximated at two scales so it stays O(n):
//   - a FINE grid (cells ~ a couple link-lengths): each node is repelled by the
//     centroid of its own + 8 neighbour cells, which spreads nearby nodes apart
//     and clears overlaps;
//   - a COARSE grid (covers the whole layout): each node is repelled by every
//     occupied coarse cell's centroid (mass-weighted), which pushes whole
//     disconnected components away from each other.
// Both grids track a DYNAMIC bounding box (recomputed each step) so the layout
// can expand freely instead of being clamped into a fixed box. There is NO
// centering force — the camera frames the result; nothing is pulled to the middle.

const WG = 64;
const GRID_C = 16; // coarse grid is GRID_C x GRID_C

export interface GpuParams {
  /** Local (fine-grid) repulsion — spreads nearby nodes, clears overlaps. */
  repulsion: number;
  /** Long-range (coarse-grid) repulsion — pushes disconnected components apart. */
  repulsionFar: number;
  /** Link strength multiplier; per-edge pull is `attraction / min(deg)`. */
  attraction: number;
  /** Velocity multiplier each step (d3 velocityDecay; 0.6 = strong damping). */
  velDecay: number;
  /** Rest length of links. */
  linkDist: number;
  /** Min squared distance for repulsion (clamps singular near-field forces). */
  distanceMin2: number;
  /** Per-node force clamp (safety against blow-ups). */
  maxForce: number;
}

export const DEFAULT_PARAMS: GpuParams = {
  repulsion: 60,
  repulsionFar: 40,
  attraction: 1.2,
  velDecay: 0.6,
  linkDist: 30,
  distanceMin2: 25,
  maxForce: 200,
};

const SHADER = /* wgsl */ `
struct Params {
  n: u32, m: u32, gridF: u32, cellsF: u32,
  cellsC: u32, _pa: u32, _pb: u32, _pc: u32,
  repulsion: f32, repulsionFar: f32, attraction: f32, velDecay: f32,
  linkDist: f32, distMin2: f32, maxForce: f32, alpha: f32,
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> edges: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> fg: array<atomic<i32>>; // fine grid: [count, sumX, sumY] per cell
@group(0) @binding(5) var<storage, read_write> cg: array<atomic<i32>>; // coarse grid: [count, sumX, sumY] per cell
@group(0) @binding(6) var<storage, read_write> fspr: array<atomic<i32>>; // 2*n spring force (fixed point)
@group(0) @binding(7) var<storage, read> deg: array<f32>;
@group(0) @binding(8) var<storage, read_write> bnd: array<atomic<i32>>; // [minX, minY, maxX, maxY] (rounded)

const SUMS: f32 = 256.0;
const FRC: f32 = 256.0;
const BIG: i32 = 1073741824;

fn boundsMin() -> vec2<f32> { return vec2<f32>(f32(atomicLoad(&bnd[0])), f32(atomicLoad(&bnd[1]))); }
fn boundsSpan() -> f32 {
  let mn = boundsMin();
  let mx = vec2<f32>(f32(atomicLoad(&bnd[2])), f32(atomicLoad(&bnd[3])));
  return max(max(mx.x - mn.x, mx.y - mn.y), 1.0);
}

// ---- per-step bounding box (dynamic, so the grids follow the spreading layout) ----
@compute @workgroup_size(${WG})
fn clearBounds(@builtin(global_invocation_id) g: vec3<u32>) {
  if (g.x != 0u) { return; }
  atomicStore(&bnd[0], BIG); atomicStore(&bnd[1], BIG);
  atomicStore(&bnd[2], -BIG); atomicStore(&bnd[3], -BIG);
}
@compute @workgroup_size(${WG})
fn bounds(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  atomicMin(&bnd[0], i32(floor(p.x))); atomicMin(&bnd[1], i32(floor(p.y)));
  atomicMax(&bnd[2], i32(ceil(p.x)));  atomicMax(&bnd[3], i32(ceil(p.y)));
}

@compute @workgroup_size(${WG})
fn clearFine(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x; if (i >= P.cellsF * 3u) { return; } atomicStore(&fg[i], 0);
}
@compute @workgroup_size(${WG})
fn clearCoarse(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x; if (i >= P.cellsC * 3u) { return; } atomicStore(&cg[i], 0);
}
@compute @workgroup_size(${WG})
fn clearForce(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x; if (i >= P.n) { return; }
  atomicStore(&fspr[2u * i], 0); atomicStore(&fspr[2u * i + 1u], 0);
}

@compute @workgroup_size(${WG})
fn scatter(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  let mn = boundsMin();
  let span = boundsSpan();
  // fine
  let csF = span / f32(P.gridF);
  let fx = clamp(i32((p.x - mn.x) / csF), 0, i32(P.gridF) - 1);
  let fy = clamp(i32((p.y - mn.y) / csF), 0, i32(P.gridF) - 1);
  let fi = (u32(fy) * P.gridF + u32(fx)) * 3u;
  atomicAdd(&fg[fi], 1);
  atomicAdd(&fg[fi + 1u], i32(((p.x - (mn.x + f32(fx) * csF)) / csF) * SUMS));
  atomicAdd(&fg[fi + 2u], i32(((p.y - (mn.y + f32(fy) * csF)) / csF) * SUMS));
  // coarse
  let csC = span / ${GRID_C}.0;
  let cx = clamp(i32((p.x - mn.x) / csC), 0, ${GRID_C} - 1);
  let cy = clamp(i32((p.y - mn.y) / csC), 0, ${GRID_C} - 1);
  let ci = (u32(cy) * ${GRID_C}u + u32(cx)) * 3u;
  atomicAdd(&cg[ci], 1);
  atomicAdd(&cg[ci + 1u], i32(((p.x - (mn.x + f32(cx) * csC)) / csC) * SUMS));
  atomicAdd(&cg[ci + 2u], i32(((p.y - (mn.y + f32(cy) * csC)) / csC) * SUMS));
}

@compute @workgroup_size(${WG})
fn spring(@builtin(global_invocation_id) g: vec3<u32>) {
  let e = g.x;
  if (e >= P.m) { return; }
  let a = edges[e].x; let b = edges[e].y;
  if (a >= P.n || b >= P.n) { return; }
  let da = max(deg[a], 1.0); let db = max(deg[b], 1.0);
  let x = pos[b] - pos[a];
  let dist = max(length(x), 1e-4);
  let l = (dist - P.linkDist) / dist * P.alpha * (P.attraction / min(da, db));
  let bias = da / (da + db);
  let fa = x * (l * (1.0 - bias));
  let fb = x * (-l * bias);
  atomicAdd(&fspr[2u * a], i32(fa.x * FRC));     atomicAdd(&fspr[2u * a + 1u], i32(fa.y * FRC));
  atomicAdd(&fspr[2u * b], i32(fb.x * FRC));     atomicAdd(&fspr[2u * b + 1u], i32(fb.y * FRC));
}

fn repelFrom(p: vec2<f32>, centroid: vec2<f32>, mass: f32, strength: f32) -> vec2<f32> {
  let d = p - centroid;
  let dist2 = max(dot(d, d), P.distMin2);
  return d * (strength * P.alpha * mass / (dist2 * sqrt(dist2)));
}

@compute @workgroup_size(${WG})
fn integrate(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  let mn = boundsMin();
  let span = boundsSpan();
  var f = vec2<f32>(0.0, 0.0);

  // Local repulsion: this node's 3x3 fine-cell neighbourhood (spread / no overlap).
  let csF = span / f32(P.gridF);
  let gx = clamp(i32((p.x - mn.x) / csF), 0, i32(P.gridF) - 1);
  let gy = clamp(i32((p.y - mn.y) / csF), 0, i32(P.gridF) - 1);
  for (var dy = -1; dy <= 1; dy = dy + 1) {
    for (var dx = -1; dx <= 1; dx = dx + 1) {
      let nx = gx + dx; let ny = gy + dy;
      if (nx < 0 || ny < 0 || nx >= i32(P.gridF) || ny >= i32(P.gridF)) { continue; }
      let fi = (u32(ny) * P.gridF + u32(nx)) * 3u;
      let k = atomicLoad(&fg[fi]);
      if (k <= 0) { continue; }
      let cm = vec2<f32>(mn.x + f32(nx) * csF, mn.y + f32(ny) * csF);
      let frac = vec2<f32>(f32(atomicLoad(&fg[fi + 1u])), f32(atomicLoad(&fg[fi + 2u]))) / SUMS / f32(k);
      f = f + repelFrom(p, cm + frac * csF, f32(k), P.repulsion);
    }
  }

  // Long-range repulsion: every occupied coarse cell (push components apart).
  let csC = span / ${GRID_C}.0;
  for (var c = 0u; c < P.cellsC; c = c + 1u) {
    let ci = c * 3u;
    let k = atomicLoad(&cg[ci]);
    if (k <= 0) { continue; }
    let cx = f32(c % ${GRID_C}u); let cy = f32(c / ${GRID_C}u);
    let cm = vec2<f32>(mn.x + cx * csC, mn.y + cy * csC);
    let frac = vec2<f32>(f32(atomicLoad(&cg[ci + 1u])), f32(atomicLoad(&cg[ci + 2u]))) / SUMS / f32(k);
    f = f + repelFrom(p, cm + frac * csC, f32(k), P.repulsionFar);
  }

  // Springs (no centering — nothing is pulled to the middle).
  f = f + vec2<f32>(f32(atomicLoad(&fspr[2u * i])), f32(atomicLoad(&fspr[2u * i + 1u]))) / FRC;

  let fl = length(f);
  if (fl > P.maxForce) { f = f * (P.maxForce / fl); }
  var v = (vel[i] + f) * P.velDecay;
  vel[i] = v;
  pos[i] = p + v;
}
`;

export interface GpuLayoutOptions {
  device: GPUDevice;
  nodeCount: number;
  /** Flat [src0,dst0, src1,dst1, ...] node indices. */
  edges: Uint32Array;
  /** Initial positions [x0,y0,...]; random spread if omitted. */
  positions?: Float32Array;
  gridDim?: number; // fine grid dimension; default scales with node count
  params?: Partial<GpuParams>;
  /** Initial alpha (default 1). Small = warm restart that keeps a settled layout. */
  alpha?: number;
  alphaDecay?: number;
  alphaMin?: number;
}

export class GpuLayout {
  readonly device: GPUDevice;
  readonly n: number;
  readonly m: number;
  readonly gridF: number;
  alpha = 1;
  private readonly alphaDecay: number;
  private readonly alphaMin: number;
  private readonly p: GpuParams;

  private readonly posBuf: GPUBuffer;
  private readonly velBuf: GPUBuffer;
  private readonly edgeBuf: GPUBuffer;
  private readonly fgBuf: GPUBuffer;
  private readonly cgBuf: GPUBuffer;
  private readonly fsprBuf: GPUBuffer;
  private readonly degBuf: GPUBuffer;
  private readonly bndBuf: GPUBuffer;
  private readonly paramBuf: GPUBuffer;
  private readonly bind: GPUBindGroup;
  private readonly pipe: Record<string, GPUComputePipeline>;

  constructor(opts: GpuLayoutOptions) {
    const { device } = opts;
    this.device = device;
    this.n = opts.nodeCount;
    this.m = opts.edges.length >>> 1;
    this.p = { ...DEFAULT_PARAMS, ...opts.params };
    this.alpha = opts.alpha ?? 1;
    this.alphaDecay = opts.alphaDecay ?? 0.985;
    this.alphaMin = opts.alphaMin ?? 0.02;
    // Fine grid ~ a few nodes per cell.
    this.gridF = Math.max(8, Math.min(opts.gridDim ?? Math.round(Math.sqrt(this.n) * 0.7), 1600));
    const cellsF = this.gridF * this.gridF;
    const cellsC = GRID_C * GRID_C;

    const ST = GPUBufferUsage.STORAGE;
    const CD = GPUBufferUsage.COPY_DST;
    const mk = (bytes: number, usage: number) => device.createBuffer({ size: Math.max(16, bytes), usage });
    this.posBuf = mk(this.n * 8, ST | CD | GPUBufferUsage.COPY_SRC);
    this.velBuf = mk(this.n * 8, ST | CD);
    this.edgeBuf = mk(this.m * 8, ST | CD);
    this.fgBuf = mk(cellsF * 12, ST | CD);
    this.cgBuf = mk(cellsC * 12, ST | CD);
    this.fsprBuf = mk(this.n * 8, ST | CD);
    this.degBuf = mk(this.n * 4, ST | CD);
    this.bndBuf = mk(16, ST | CD);
    this.paramBuf = device.createBuffer({ size: 64, usage: GPUBufferUsage.UNIFORM | CD });

    const pos = opts.positions ?? randomSpread(this.n, Math.max(60, this.p.linkDist * Math.sqrt(this.n) * 0.5));
    device.queue.writeBuffer(this.posBuf, 0, pos as BufferSource);
    device.queue.writeBuffer(this.velBuf, 0, new Float32Array(this.n * 2));
    if (this.m > 0) device.queue.writeBuffer(this.edgeBuf, 0, opts.edges as BufferSource);
    const deg = new Float32Array(this.n);
    for (let i = 0; i < this.m; i++) {
      deg[opts.edges[2 * i]]++;
      deg[opts.edges[2 * i + 1]]++;
    }
    device.queue.writeBuffer(this.degBuf, 0, deg);

    const mod = device.createShaderModule({ code: SHADER });
    const layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: "uniform" } },
        ...[1, 2].map((binding) => ({ binding, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } })),
        { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: "read-only-storage" as const } },
        ...[4, 5, 6].map((binding) => ({ binding, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } })),
        { binding: 7, visibility: GPUShaderStage.COMPUTE, buffer: { type: "read-only-storage" as const } },
        { binding: 8, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } },
      ],
    });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    this.pipe = {};
    for (const ep of ["clearBounds", "bounds", "clearFine", "clearCoarse", "clearForce", "scatter", "spring", "integrate"]) {
      this.pipe[ep] = device.createComputePipeline({ layout: pl, compute: { module: mod, entryPoint: ep } });
    }
    this.bind = device.createBindGroup({
      layout,
      entries: [
        { binding: 0, resource: { buffer: this.paramBuf } },
        { binding: 1, resource: { buffer: this.posBuf } },
        { binding: 2, resource: { buffer: this.velBuf } },
        { binding: 3, resource: { buffer: this.edgeBuf } },
        { binding: 4, resource: { buffer: this.fgBuf } },
        { binding: 5, resource: { buffer: this.cgBuf } },
        { binding: 6, resource: { buffer: this.fsprBuf } },
        { binding: 7, resource: { buffer: this.degBuf } },
        { binding: 8, resource: { buffer: this.bndBuf } },
      ],
    });
    this.writeParams();
  }

  reheat(alpha = 0.6) {
    this.alpha = Math.max(this.alpha, alpha);
  }
  get positions(): GPUBuffer {
    return this.posBuf;
  }
  get edgeBuffer(): GPUBuffer {
    return this.edgeBuf;
  }
  pin(index: number, x: number, y: number) {
    this.device.queue.writeBuffer(this.posBuf, index * 8, new Float32Array([x, y]));
    this.device.queue.writeBuffer(this.velBuf, index * 8, new Float32Array([0, 0]));
  }

  private writeParams() {
    const ab = new ArrayBuffer(64);
    const u = new Uint32Array(ab);
    const f = new Float32Array(ab);
    u[0] = this.n;
    u[1] = this.m;
    u[2] = this.gridF;
    u[3] = this.gridF * this.gridF;
    u[4] = GRID_C * GRID_C;
    f[8] = this.p.repulsion;
    f[9] = this.p.repulsionFar;
    f[10] = this.p.attraction;
    f[11] = this.p.velDecay;
    f[12] = this.p.linkDist;
    f[13] = this.p.distanceMin2;
    f[14] = this.p.maxForce;
    f[15] = this.alpha;
    this.device.queue.writeBuffer(this.paramBuf, 0, ab);
  }

  step(iterations = 1) {
    const cellsF = this.gridF * this.gridF;
    const cellsC = GRID_C * GRID_C;
    const enc = this.device.createCommandEncoder();
    for (let it = 0; it < iterations; it++) {
      this.writeParams();
      const pass = enc.beginComputePass();
      pass.setBindGroup(0, this.bind);
      const run = (name: string, count: number) => {
        pass.setPipeline(this.pipe[name]);
        pass.dispatchWorkgroups(Math.ceil(count / WG));
      };
      run("clearBounds", 1);
      run("bounds", this.n);
      run("clearFine", cellsF * 3);
      run("clearCoarse", cellsC * 3);
      run("clearForce", this.n);
      run("scatter", this.n);
      if (this.m > 0) run("spring", this.m);
      run("integrate", this.n);
      pass.end();
      this.alpha = Math.max(this.alphaMin, this.alpha * this.alphaDecay);
    }
    this.device.queue.submit([enc.finish()]);
  }

  async readPositions(): Promise<Float32Array> {
    const bytes = this.n * 8;
    const staging = this.device.createBuffer({ size: bytes, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
    const enc = this.device.createCommandEncoder();
    enc.copyBufferToBuffer(this.posBuf, 0, staging, 0, bytes);
    this.device.queue.submit([enc.finish()]);
    await staging.mapAsync(GPUMapMode.READ);
    const out = new Float32Array(staging.getMappedRange().slice(0));
    staging.unmap();
    staging.destroy();
    return out;
  }

  destroy() {
    for (const b of [this.posBuf, this.velBuf, this.edgeBuf, this.fgBuf, this.cgBuf, this.fsprBuf, this.degBuf, this.bndBuf, this.paramBuf]) {
      b.destroy();
    }
  }
}


function randomSpread(n: number, half: number): Float32Array {
  const a = new Float32Array(n * 2);
  let s = 0x2545f491;
  const rnd = () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return ((s >>> 0) / 0xffffffff) * 2 - 1;
  };
  for (let i = 0; i < n * 2; i++) a[i] = rnd() * half;
  return a;
}
