/// <reference types="@webgpu/types" />
// GPU port of d3-force, computed EXACTLY (not approximated). Each step reproduces
// d3's tick: apply charge (many-body repulsion), then link (springs, using the
// post-charge predicted velocity), then a weak forceX/forceY centering, then
// `vx = (vx + forces) * velocityDecay; x += vx`. Velocity carries across ticks
// and alpha cools like d3. Repulsion here is the exact all-pairs sum, which is
// d3.forceManyBody().theta(0) — verified against d3 itself in test/gpu-oracle.ts.
// (Barnes-Hut acceleration, matching d3's default theta, is layered on top of this
//  exact baseline separately.)
//
// d3 reference (node_modules/d3-force/src):
//   manyBody.apply:  vx += (qx - x) * value * alpha / l       (l = dx*dx+dy*dy)
//                    if (l < distanceMin2) l = sqrt(distanceMin2 * l)
//   link.force:      x = (tx+tvx) - (sx+svx);  l = (|.|-dist)/|.| * alpha * strength
//                    target.vx -= x*bias;  source.vx += x*(1-bias);  bias = degS/(degS+degT)
//   forceX/forceY:   vx += (0 - x) * strength * alpha
//   tick:            alpha += (0 - alpha)*alphaDecay; forces(); vx = (vx)*velocityDecay; x += vx

const WG = 64;

export interface GpuParams {
  /** Many-body strength (d3 forceManyBody strength; negative = repulsion). */
  charge: number;
  /** Link rest length (d3 forceLink distance). */
  linkDistance: number;
  /**
   * Link strength multiplier on top of d3's default 1/min(deg) (so 1.0 == stock
   * d3 forceLink). The degree normalization keeps the parallel solver stable;
   * don't replace it with a constant or high-degree nodes explode.
   */
  linkStrength: number;
  /** forceX/forceY strength toward the origin (weak centering, as the app used). */
  center: number;
  /** Velocity multiplier each step (= 1 - d3 velocityDecay; d3 default 0.4 -> 0.6). */
  velDecay: number;
  /** d3 forceManyBody distanceMin (stored squared); clamps near-field. */
  distanceMin2: number;
}

// Matches the app's previous d3-force setup.
export const DEFAULT_PARAMS: GpuParams = {
  charge: -240,
  linkDistance: 38,
  linkStrength: 1, // multiplier on d3's default 1/min(deg)
  center: 0.045,
  velDecay: 0.6,
  distanceMin2: 1,
};

const FRC = 4096.0; // fixed-point scale for atomic link-force accumulation

const SHADER = /* wgsl */ `
struct Params {
  n: u32, m: u32, _a: u32, _b: u32,
  charge: f32, linkDist: f32, linkStrength: f32, center: f32,
  velDecay: f32, distMin2: f32, alpha: f32, _c: f32,
};
@group(0) @binding(0) var<uniform> P: Params;
@group(0) @binding(1) var<storage, read_write> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> edges: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> lf: array<atomic<i32>>; // 2*n link force (fixed point)
@group(0) @binding(5) var<storage, read> deg: array<f32>;

const FRC: f32 = ${FRC};

@compute @workgroup_size(${WG})
fn clearForce(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x; if (i >= P.n) { return; }
  atomicStore(&lf[2u * i], 0); atomicStore(&lf[2u * i + 1u], 0);
}

// d3 forceManyBody, exact (theta = 0): sum over every other node, vx += x*value*alpha/l.
@compute @workgroup_size(${WG})
fn charge(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  var acc = vec2<f32>(0.0, 0.0);
  for (var j = 0u; j < P.n; j = j + 1u) {
    if (j == i) { continue; }
    let q = pos[j];
    var x = q.x - p.x;
    var y = q.y - p.y;
    var l = x * x + y * y;
    if (l == 0.0) { continue; } // coincident: d3 jiggles; seeds avoid exact overlap
    if (l < P.distMin2) { l = sqrt(P.distMin2 * l); }
    let w = P.charge * P.alpha / l;
    acc.x = acc.x + x * w;
    acc.y = acc.y + y * w;
  }
  vel[i] = vel[i] + acc; // carry velocity + charge
}

// Fixed-point with saturation: a far-flung outlier can produce an enormous link
// force; clamp before the i32 cast so the atomic can never wrap (which would
// yeet a node to infinity) — it saturates to a large value instead.
fn fp(v: f32) -> i32 { return i32(clamp(v * FRC, -2.0e9, 2.0e9)); }

// d3 forceLink: uses predicted positions (pos + post-charge vel), degree bias,
// and d3's DEFAULT strength 1/min(deg). The degree normalization is what keeps
// the *parallel* (Jacobi) relaxation stable: a high-degree node summing many
// simultaneous link corrections would otherwise overshoot and diverge (d3's
// serial Gauss-Seidel tolerates a constant strength; a parallel solver can't).
@compute @workgroup_size(${WG})
fn link(@builtin(global_invocation_id) g: vec3<u32>) {
  let e = g.x;
  if (e >= P.m) { return; }
  let a = edges[e].x; let b = edges[e].y;
  if (a >= P.n || b >= P.n) { return; }
  let da = deg[a]; let db = deg[b];
  let x = (pos[b].x + vel[b].x) - (pos[a].x + vel[a].x);
  let y = (pos[b].y + vel[b].y) - (pos[a].y + vel[a].y);
  let len = sqrt(x * x + y * y);
  if (len == 0.0) { return; }
  let st = P.linkStrength / min(da, db); // d3 default link strength = 1/min(count)
  let l = (len - P.linkDist) / len * P.alpha * st;
  let fx = x * l; let fy = y * l;
  let bias = da / (da + db); // source = a
  // target b: vx -= f*bias ; source a: vx += f*(1-bias)
  atomicAdd(&lf[2u * b], fp(-fx * bias));
  atomicAdd(&lf[2u * b + 1u], fp(-fy * bias));
  atomicAdd(&lf[2u * a], fp(fx * (1.0 - bias)));
  atomicAdd(&lf[2u * a + 1u], fp(fy * (1.0 - bias)));
}

// Add link force + forceX/forceY centering, then d3's integrate.
@compute @workgroup_size(${WG})
fn integrate(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x;
  if (i >= P.n) { return; }
  let p = pos[i];
  var v = vel[i];
  v = v + vec2<f32>(f32(atomicLoad(&lf[2u * i])), f32(atomicLoad(&lf[2u * i + 1u]))) / FRC;
  v = v + (vec2<f32>(0.0, 0.0) - p) * (P.center * P.alpha); // forceX/forceY
  v = v * P.velDecay;
  vel[i] = v;
  pos[i] = p + v;
}
`;

export interface GpuLayoutOptions {
  device: GPUDevice;
  nodeCount: number;
  edges: Uint32Array; // flat [src,dst,...]
  positions?: Float32Array;
  params?: Partial<GpuParams>;
  /** Initial alpha (default 1). */
  alpha?: number;
  /** d3 alpha decay rate (per step alpha += (0-alpha)*rate). Default ~300-step. */
  alphaDecay?: number;
  alphaMin?: number;
}

export class GpuLayout {
  readonly device: GPUDevice;
  readonly n: number;
  readonly m: number;
  alpha = 1;
  private readonly alphaDecay: number;
  private readonly alphaMin: number;
  private readonly p: GpuParams;

  private readonly posBuf: GPUBuffer;
  private readonly velBuf: GPUBuffer;
  private readonly edgeBuf: GPUBuffer;
  private readonly lfBuf: GPUBuffer;
  private readonly degBuf: GPUBuffer;
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
    this.alphaDecay = opts.alphaDecay ?? 1 - Math.pow(0.001, 1 / 300);
    this.alphaMin = opts.alphaMin ?? 0.001;

    const ST = GPUBufferUsage.STORAGE;
    const CD = GPUBufferUsage.COPY_DST;
    const mk = (bytes: number, usage: number) => device.createBuffer({ size: Math.max(16, bytes), usage });
    this.posBuf = mk(this.n * 8, ST | CD | GPUBufferUsage.COPY_SRC);
    this.velBuf = mk(this.n * 8, ST | CD);
    this.edgeBuf = mk(this.m * 8, ST | CD);
    this.lfBuf = mk(this.n * 8, ST | CD);
    this.degBuf = mk(this.n * 4, ST | CD);
    this.paramBuf = device.createBuffer({ size: 48, usage: GPUBufferUsage.UNIFORM | CD });

    const pos = opts.positions ?? phyllotaxis(this.n, this.p.linkDistance);
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
        { binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: "storage" as const } },
        { binding: 5, visibility: GPUShaderStage.COMPUTE, buffer: { type: "read-only-storage" as const } },
      ],
    });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    this.pipe = {};
    for (const ep of ["clearForce", "charge", "link", "integrate"]) {
      this.pipe[ep] = device.createComputePipeline({ layout: pl, compute: { module: mod, entryPoint: ep } });
    }
    this.bind = device.createBindGroup({
      layout,
      entries: [
        { binding: 0, resource: { buffer: this.paramBuf } },
        { binding: 1, resource: { buffer: this.posBuf } },
        { binding: 2, resource: { buffer: this.velBuf } },
        { binding: 3, resource: { buffer: this.edgeBuf } },
        { binding: 4, resource: { buffer: this.lfBuf } },
        { binding: 5, resource: { buffer: this.degBuf } },
      ],
    });
    this.writeParams();
  }

  reheat(alpha = 0.6) {
    this.alpha = Math.max(this.alpha, alpha);
  }
  /** True once cooled past alphaMin — d3 stops ticking here. */
  get settled(): boolean {
    return this.alpha < this.alphaMin;
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
    const ab = new ArrayBuffer(48);
    const u = new Uint32Array(ab);
    const f = new Float32Array(ab);
    u[0] = this.n;
    u[1] = this.m;
    f[4] = this.p.charge;
    f[5] = this.p.linkDistance;
    f[6] = this.p.linkStrength;
    f[7] = this.p.center;
    f[8] = this.p.velDecay;
    f[9] = this.p.distanceMin2;
    f[10] = this.alpha;
    this.device.queue.writeBuffer(this.paramBuf, 0, ab);
  }

  step(iterations = 1) {
    if (this.alpha < this.alphaMin) return; // settled — d3 stops ticking at alphaMin
    const enc = this.device.createCommandEncoder();
    for (let it = 0; it < iterations; it++) {
      this.alpha += (0 - this.alpha) * this.alphaDecay; // d3 cooling, before applying forces
      this.writeParams();
      const pass = enc.beginComputePass();
      pass.setBindGroup(0, this.bind);
      const run = (name: string, count: number) => {
        pass.setPipeline(this.pipe[name]);
        pass.dispatchWorkgroups(Math.ceil(count / WG));
      };
      run("clearForce", this.n);
      run("charge", this.n);
      if (this.m > 0) run("link", this.m);
      run("integrate", this.n);
      pass.end();
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
    for (const b of [this.posBuf, this.velBuf, this.edgeBuf, this.lfBuf, this.degBuf, this.paramBuf]) b.destroy();
  }
}

// d3's default node placement (phyllotaxis spiral), used when no seed is given.
function phyllotaxis(n: number, radius: number): Float32Array {
  const a = new Float32Array(n * 2);
  const r = radius * 1.2;
  const angle = Math.PI * (3 - Math.sqrt(5));
  for (let i = 0; i < n; i++) {
    const rad = r * Math.sqrt(0.5 + i);
    const ang = i * angle;
    a[2 * i] = rad * Math.cos(ang);
    a[2 * i + 1] = rad * Math.sin(ang);
  }
  return a;
}
