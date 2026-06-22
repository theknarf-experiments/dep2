/// <reference types="@webgpu/types" />
// GPU force-directed layout (WebGPU compute). Pure WebGPU — no DOM — so it runs
// in the browser and in Deno (for headless verification). Positions live in a GPU
// buffer the whole time; a renderer can bind it directly (no CPU round-trip),
// which is what makes millions of nodes viable.
//
// Repulsion uses a coarse uniform grid (a particle-mesh approximation): each
// step bins nodes into cells accumulating a centroid + mass, then every node is
// repelled by all occupied cell centroids weighted by mass. That's O(cells·n)
// instead of O(n²) — cells is a small constant — plus O(n) springs and O(n)
// integrate. Spring/centre/repulsion forces are summed then integrated with
// velocity damping and a decaying alpha (a cooling schedule), like d3-force.

const WG = 64; // workgroup size

export interface GpuParams {
  repulsion: number;
  attraction: number;
  center: number;
  velDecay: number;
  dt: number;
  linkDist: number;
  maxForce: number;
}

export const DEFAULT_PARAMS: GpuParams = {
  repulsion: 90,
  attraction: 0.6,
  center: 0.02,
  velDecay: 0.6,
  dt: 0.85,
  linkDist: 30,
  maxForce: 800,
};

const SHADER = /* wgsl */ `
struct Params {
  n: u32, m: u32, gridDim: u32, cells: u32,
  worldHalf: f32, cellSize: f32, repulsion: f32, attraction: f32,
  center: f32, velDecay: f32, dt: f32, alpha: f32,
  linkDist: f32, maxForce: f32, _p0: f32, _p1: f32,
};

@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> edges: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> cnt: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> psum: array<atomic<i32>>; // 2*cells, centroid fraction
@group(0) @binding(6) var<storage, read_write> fspr: array<atomic<i32>>; // 2*n, spring force (fixed point)

const SUMS: f32 = 256.0; // fixed-point scale for cell centroid fractions
const FRC: f32 = 256.0;  // fixed-point scale for spring forces

fn cellCoord(p: vec2<f32>) -> vec2<i32> {
  let g = i32(P.gridDim);
  let cx = clamp(i32((p.x + P.worldHalf) / P.cellSize), 0, g - 1);
  let cy = clamp(i32((p.y + P.worldHalf) / P.cellSize), 0, g - 1);
  return vec2<i32>(cx, cy);
}
fn cellMin(c: vec2<i32>) -> vec2<f32> {
  return vec2<f32>(-P.worldHalf + f32(c.x) * P.cellSize, -P.worldHalf + f32(c.y) * P.cellSize);
}

@compute @workgroup_size(${WG})
fn clearGrid(@builtin(global_invocation_id) gid: vec3<u32>) {
  let i = gid.x;
  if (i >= P.cells) { return; }
  atomicStore(&cnt[i], 0u);
  atomicStore(&psum[2u * i], 0);
  atomicStore(&psum[2u * i + 1u], 0);
}

@compute @workgroup_size(${WG})
fn clearForce(@builtin(global_invocation_id) gid: vec3<u32>) {
  let i = gid.x;
  if (i >= P.n) { return; }
  atomicStore(&fspr[2u * i], 0);
  atomicStore(&fspr[2u * i + 1u], 0);
}

@compute @workgroup_size(${WG})
fn scatter(@builtin(global_invocation_id) gid: vec3<u32>) {
  let i = gid.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  let c = cellCoord(p);
  let idx = u32(c.y) * P.gridDim + u32(c.x);
  let frac = (p - cellMin(c)) / P.cellSize; // [0,1]
  atomicAdd(&cnt[idx], 1u);
  atomicAdd(&psum[2u * idx], i32(frac.x * SUMS));
  atomicAdd(&psum[2u * idx + 1u], i32(frac.y * SUMS));
}

@compute @workgroup_size(${WG})
fn spring(@builtin(global_invocation_id) gid: vec3<u32>) {
  let e = gid.x;
  if (e >= P.m) { return; }
  let a = edges[e].x;
  let b = edges[e].y;
  if (a >= P.n || b >= P.n) { return; }
  let d = pos[b] - pos[a];
  let dist = max(length(d), 1e-4);
  let dir = d / dist;
  let f = dir * ((dist - P.linkDist) * P.attraction * P.alpha);
  atomicAdd(&fspr[2u * a], i32(f.x * FRC));
  atomicAdd(&fspr[2u * a + 1u], i32(f.y * FRC));
  atomicAdd(&fspr[2u * b], i32(-f.x * FRC));
  atomicAdd(&fspr[2u * b + 1u], i32(-f.y * FRC));
}

@compute @workgroup_size(${WG})
fn integrate(@builtin(global_invocation_id) gid: vec3<u32>) {
  let i = gid.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  var f = vec2<f32>(0.0, 0.0);

  // Repulsion from every occupied cell centroid (mass-weighted particle mesh).
  let g = i32(P.gridDim);
  for (var cy = 0; cy < g; cy = cy + 1) {
    for (var cx = 0; cx < g; cx = cx + 1) {
      let idx = u32(cy) * P.gridDim + u32(cx);
      let k = atomicLoad(&cnt[idx]);
      if (k == 0u) { continue; }
      let fr = vec2<f32>(f32(atomicLoad(&psum[2u * idx])), f32(atomicLoad(&psum[2u * idx + 1u]))) / SUMS / f32(k);
      let centroid = cellMin(vec2<i32>(cx, cy)) + fr * P.cellSize;
      let d = p - centroid;
      let dist2 = dot(d, d);
      if (dist2 < 1e-2) { continue; }
      let invr = inverseSqrt(dist2);
      f = f + d * (P.repulsion * P.alpha * f32(k) * invr / dist2);
    }
  }

  // Centering + springs.
  f = f - p * (P.center * P.alpha);
  f = f + vec2<f32>(f32(atomicLoad(&fspr[2u * i])), f32(atomicLoad(&fspr[2u * i + 1u]))) / FRC;

  // Clamp, integrate with damping.
  let fl = length(f);
  if (fl > P.maxForce) { f = f * (P.maxForce / fl); }
  var v = (vel[i] + f * P.dt) * P.velDecay;
  vel[i] = v;
  pos[i] = p + v * P.dt;
}
`;

export interface GpuLayoutOptions {
  device: GPUDevice;
  nodeCount: number;
  /** Flat [src0,dst0, src1,dst1, ...] node indices. */
  edges: Uint32Array;
  /** Initial positions [x0,y0,...]; random spread if omitted. */
  positions?: Float32Array;
  gridDim?: number;
  worldHalf?: number;
  params?: Partial<GpuParams>;
  /** Cooling: alpha *= alphaDecay each step, floored at alphaMin. */
  alphaDecay?: number;
  alphaMin?: number;
}

/** A GPU force simulation. Owns its buffers; `step()` advances it, `positions`
 *  is the live GPU buffer (bind it in a renderer), `readPositions()` copies back. */
export class GpuLayout {
  readonly device: GPUDevice;
  readonly n: number;
  readonly m: number;
  readonly gridDim: number;
  readonly worldHalf: number;
  alpha = 1;
  private readonly alphaDecay: number;
  private readonly alphaMin: number;
  private readonly p: GpuParams;

  private readonly posBuf: GPUBuffer;
  private readonly velBuf: GPUBuffer;
  private readonly edgeBuf: GPUBuffer;
  private readonly cntBuf: GPUBuffer;
  private readonly sumBuf: GPUBuffer;
  private readonly fsprBuf: GPUBuffer;
  private readonly paramBuf: GPUBuffer;
  private readonly bind: GPUBindGroup;
  private readonly pipe: Record<string, GPUComputePipeline>;

  constructor(opts: GpuLayoutOptions) {
    const { device } = opts;
    this.device = device;
    this.n = opts.nodeCount;
    this.m = opts.edges.length >>> 1;
    this.p = { ...DEFAULT_PARAMS, ...opts.params };
    this.gridDim = opts.gridDim ?? 32;
    this.worldHalf = opts.worldHalf ?? Math.max(120, this.p.linkDist * Math.sqrt(this.n) * 0.6);
    this.alphaDecay = opts.alphaDecay ?? 0.985;
    this.alphaMin = opts.alphaMin ?? 0.02;
    const cells = this.gridDim * this.gridDim;

    const mk = (bytes: number, usage: number) =>
      device.createBuffer({ size: Math.max(16, bytes), usage });
    const ST = GPUBufferUsage.STORAGE;
    const CD = GPUBufferUsage.COPY_DST;
    this.posBuf = mk(this.n * 8, ST | CD | GPUBufferUsage.COPY_SRC);
    this.velBuf = mk(this.n * 8, ST | CD);
    this.edgeBuf = mk(this.m * 8, ST | CD);
    this.cntBuf = mk(cells * 4, ST | CD);
    this.sumBuf = mk(cells * 8, ST | CD);
    this.fsprBuf = mk(this.n * 8, ST | CD);
    this.paramBuf = device.createBuffer({ size: 64, usage: GPUBufferUsage.UNIFORM | CD });

    // Seed positions (spread over the world so cells start sparse) + zero vel.
    const pos = opts.positions ?? randomSpread(this.n, this.worldHalf * 0.7);
    device.queue.writeBuffer(this.posBuf, 0, pos as BufferSource);
    device.queue.writeBuffer(this.velBuf, 0, new Float32Array(this.n * 2));
    if (this.m > 0) device.queue.writeBuffer(this.edgeBuf, 0, opts.edges as BufferSource);

    const mod = device.createShaderModule({ code: SHADER });
    const layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: "uniform" } },
        ...[1, 2].map((binding) => ({ binding, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } })),
        { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: "read-only-storage" as const } },
        ...[4, 5, 6].map((binding) => ({ binding, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } })),
      ],
    });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    this.pipe = {};
    for (const entryPoint of ["clearGrid", "clearForce", "scatter", "spring", "integrate"]) {
      this.pipe[entryPoint] = device.createComputePipeline({ layout: pl, compute: { module: mod, entryPoint } });
    }
    this.bind = device.createBindGroup({
      layout,
      entries: [
        { binding: 0, resource: { buffer: this.paramBuf } },
        { binding: 1, resource: { buffer: this.posBuf } },
        { binding: 2, resource: { buffer: this.velBuf } },
        { binding: 3, resource: { buffer: this.edgeBuf } },
        { binding: 4, resource: { buffer: this.cntBuf } },
        { binding: 5, resource: { buffer: this.sumBuf } },
        { binding: 6, resource: { buffer: this.fsprBuf } },
      ],
    });
    this.writeParams();
  }

  /** Re-heat the cooling schedule (e.g. on drag or a new dataset). */
  reheat(alpha = 0.6) {
    this.alpha = Math.max(this.alpha, alpha);
  }

  /** The live positions buffer ([x,y] per node) — bind it directly in a renderer. */
  get positions(): GPUBuffer {
    return this.posBuf;
  }

  /** The edges buffer ([src,dst] u32 per edge) — a renderer can draw from it. */
  get edgeBuffer(): GPUBuffer {
    return this.edgeBuf;
  }

  /** Pin a node to a world position (for dragging): overwrite its slot and
   *  zero velocity. Call after `step()` each frame while dragging. */
  pin(index: number, x: number, y: number) {
    this.device.queue.writeBuffer(this.posBuf, index * 8, new Float32Array([x, y]));
    this.device.queue.writeBuffer(this.velBuf, index * 8, new Float32Array([0, 0]));
  }

  private writeParams() {
    const cells = this.gridDim * this.gridDim;
    const buf = new ArrayBuffer(64);
    const u = new Uint32Array(buf);
    const f = new Float32Array(buf);
    u[0] = this.n;
    u[1] = this.m;
    u[2] = this.gridDim;
    u[3] = cells;
    f[4] = this.worldHalf;
    f[5] = (2 * this.worldHalf) / this.gridDim;
    f[6] = this.p.repulsion;
    f[7] = this.p.attraction;
    f[8] = this.p.center;
    f[9] = this.p.velDecay;
    f[10] = this.p.dt;
    f[11] = this.alpha;
    f[12] = this.p.linkDist;
    f[13] = this.p.maxForce;
    this.device.queue.writeBuffer(this.paramBuf, 0, buf);
  }

  /** Advance the simulation `iterations` steps. */
  step(iterations = 1) {
    const cells = this.gridDim * this.gridDim;
    const enc = this.device.createCommandEncoder();
    for (let it = 0; it < iterations; it++) {
      this.writeParams();
      const pass = enc.beginComputePass();
      pass.setBindGroup(0, this.bind);
      const run = (name: string, count: number) => {
        pass.setPipeline(this.pipe[name]);
        pass.dispatchWorkgroups(Math.ceil(count / WG));
      };
      run("clearGrid", cells);
      run("clearForce", this.n);
      run("scatter", this.n);
      if (this.m > 0) run("spring", this.m);
      run("integrate", this.n);
      pass.end();
      this.alpha = Math.max(this.alphaMin, this.alpha * this.alphaDecay);
    }
    this.device.queue.submit([enc.finish()]);
  }

  /** Copy positions back to the CPU (for tests / export; not the render path). */
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
    for (const b of [this.posBuf, this.velBuf, this.edgeBuf, this.cntBuf, this.sumBuf, this.fsprBuf, this.paramBuf]) {
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
