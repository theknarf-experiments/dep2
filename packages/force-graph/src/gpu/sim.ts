/// <reference types="@webgpu/types" />
// GPU port of d3-force. Each step reproduces d3's tick: charge (many-body
// repulsion), then links (springs, using the post-charge predicted velocity),
// then a weak forceX/forceY centering, then `vx = (vx + forces) * velocityDecay;
// x += vx`. Velocity carries across ticks and alpha cools like d3.
//
// Charge has two implementations, selected by size:
//   * EXACT all-pairs (O(n^2)) — identical to d3.forceManyBody().theta(0).
//     Verified bit-exact vs d3 in test/gpu-oracle.ts. Used for small graphs.
//   * BARNES-HUT (O(n log n)) — a quadtree built on the GPU and traversed with
//     d3's exact theta criterion (w*w/theta2 < dist2), same charge force law.
//     This is what scales to millions. Verified against the exact path (and d3)
//     in test/gpu-bh.ts. The tree is a regular pyramid (no pointers/sorting, so
//     nothing can deadlock under WGSL's memory model): scatter each node into its
//     finest cell with atomics, reduce centre-of-mass/mass up the levels, then
//     each node walks the pyramid from the root applying theta.
//
// d3 reference (node_modules/d3-force/src):
//   manyBody.apply:  vx += (qx - x) * value * alpha / l    (l = dx*dx+dy*dy)
//                    if (l < distanceMin2) l = sqrt(distanceMin2 * l)
//                    Barnes-Hut: treat a quad as one body when w*w/theta2 < l
//   link.force:      x = (tx+tvx) - (sx+svx); l = (|.|-dist)/|.| * alpha * strength
//   forceX/forceY:   vx += (0 - x) * strength * alpha

const WG = 64;
const FRC = 4096.0; // fixed-point scale for atomic accumulation

export interface GpuParams {
  /** Many-body strength (d3 forceManyBody strength; negative = repulsion). */
  charge: number;
  /** Link rest length (d3 forceLink distance). */
  linkDistance: number;
  /**
   * Link strength multiplier on top of d3's default 1/min(deg) (so 1.0 == stock
   * d3 forceLink). The degree normalization keeps the parallel solver stable.
   */
  linkStrength: number;
  /** forceX/forceY strength toward the origin (weak centering). */
  center: number;
  /** Velocity multiplier each step (= 1 - d3 velocityDecay; d3 default 0.4 -> 0.6). */
  velDecay: number;
  /** d3 forceManyBody distanceMin (stored squared); clamps near-field. */
  distanceMin2: number;
  /** Barnes-Hut accuracy (d3 default 0.9). Lower = more accurate, slower. */
  theta: number;
}

export const DEFAULT_PARAMS: GpuParams = {
  charge: -240,
  linkDistance: 38,
  linkStrength: 1, // multiplier on d3's default 1/min(deg)
  center: 0.045,
  velDecay: 0.6,
  distanceMin2: 1,
  theta: 0.9,
};

const PRELUDE = /* wgsl */ `
struct Params {
  n: u32, m: u32, maxLevel: u32, gridRes: u32,
  charge: f32, linkDist: f32, linkStrength: f32, center: f32,
  velDecay: f32, distMin2: f32, alpha: f32, theta2: f32,
};
@group(0) @binding(0) var<uniform> P: Params;
const FRC: f32 = ${FRC};
fn fp(v: f32) -> i32 { return i32(clamp(v * FRC, -2.0e9, 2.0e9)); }
// Offset of a pyramid level in the flat cell arrays: sum of 4^l for l < level.
fn levelOffset(level: u32) -> u32 {
  var o = 0u; var l = 0u;
  loop { if (l >= level) { break; } o = o + (1u << (2u * l)); l = l + 1u; }
  return o;
}
// Square layout region from the (integer) bounding box: returns (originX, originY, size).
fn geom(b0: i32, b1: i32, b2: i32, b3: i32) -> vec3<f32> {
  let minx = f32(b0); let miny = f32(b1); let maxx = f32(b2); let maxy = f32(b3);
  var size = max(maxx - minx, maxy - miny);
  size = max(size, 1.0);
  let pad = size * 0.01;
  return vec3<f32>(minx - pad, miny - pad, size + 2.0 * pad);
}
`;

// ---- per-pass shaders (each declares only the buffers it binds, from binding 1) ----

const CLEAR_FORCE = /* wgsl */ `
@group(0) @binding(1) var<storage, read_write> lf: array<atomic<i32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  atomicStore(&lf[2u * i], 0); atomicStore(&lf[2u * i + 1u], 0);
}`;

const CHARGE_EXACT = /* wgsl */ `
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  let p = pos[i];
  var acc = vec2<f32>(0.0, 0.0);
  for (var j = 0u; j < P.n; j = j + 1u) {
    if (j == i) { continue; }
    let q = pos[j];
    let x = q.x - p.x; let y = q.y - p.y;
    var l = x * x + y * y;
    if (l == 0.0) { continue; }
    if (l < P.distMin2) { l = sqrt(P.distMin2 * l); }
    let w = P.charge * P.alpha / l;
    acc.x = acc.x + x * w; acc.y = acc.y + y * w;
  }
  vel[i] = vel[i] + acc;
}`;

const CLEAR_BBOX = /* wgsl */ `
@group(0) @binding(1) var<storage, read_write> bbox: array<atomic<i32>>;
@compute @workgroup_size(4) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= 4u) { return; }
  if (i < 2u) { atomicStore(&bbox[i], 0x7fffffff); } else { atomicStore(&bbox[i], -2147483647); }
}`;

const BBOX = /* wgsl */ `
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> bbox: array<atomic<i32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  let p = pos[i];
  atomicMin(&bbox[0], i32(floor(p.x))); atomicMin(&bbox[1], i32(floor(p.y)));
  atomicMax(&bbox[2], i32(ceil(p.x)));  atomicMax(&bbox[3], i32(ceil(p.y)));
}`;

const CLEAR_SCATTER = /* wgsl */ `
@group(0) @binding(1) var<storage, read_write> sMass: array<atomic<i32>>;
@group(0) @binding(2) var<storage, read_write> sSumX: array<atomic<i32>>;
@group(0) @binding(3) var<storage, read_write> sSumY: array<atomic<i32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; let fin = P.gridRes * P.gridRes; if (i >= fin) { return; }
  atomicStore(&sMass[i], 0); atomicStore(&sSumX[i], 0); atomicStore(&sSumY[i], 0);
}`;

const SCATTER = /* wgsl */ `
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> bbox: array<i32>;
@group(0) @binding(3) var<storage, read_write> sMass: array<atomic<i32>>;
@group(0) @binding(4) var<storage, read_write> sSumX: array<atomic<i32>>;
@group(0) @binding(5) var<storage, read_write> sSumY: array<atomic<i32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  let p = pos[i];
  let gg = geom(bbox[0], bbox[1], bbox[2], bbox[3]);
  let R = P.gridRes; let cs = gg.z / f32(R);
  let cx = clamp(u32((p.x - gg.x) / gg.z * f32(R)), 0u, R - 1u);
  let cy = clamp(u32((p.y - gg.y) / gg.z * f32(R)), 0u, R - 1u);
  let ccx = gg.x + (f32(cx) + 0.5) * cs;
  let ccy = gg.y + (f32(cy) + 0.5) * cs;
  let cell = cy * R + cx;
  atomicAdd(&sMass[cell], 1);
  atomicAdd(&sSumX[cell], i32(round((p.x - ccx) * FRC)));
  atomicAdd(&sSumY[cell], i32(round((p.y - ccy) * FRC)));
}`;

const FINALIZE = /* wgsl */ `
@group(0) @binding(1) var<storage, read> bbox: array<i32>;
@group(0) @binding(2) var<storage, read> sMass: array<i32>;
@group(0) @binding(3) var<storage, read> sSumX: array<i32>;
@group(0) @binding(4) var<storage, read> sSumY: array<i32>;
@group(0) @binding(5) var<storage, read_write> pMass: array<f32>;
@group(0) @binding(6) var<storage, read_write> pComX: array<f32>;
@group(0) @binding(7) var<storage, read_write> pComY: array<f32>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; let R = P.gridRes; let fin = R * R; if (i >= fin) { return; }
  let off = levelOffset(P.maxLevel);
  let mi = sMass[i];
  if (mi == 0) { pMass[off + i] = 0.0; pComX[off + i] = 0.0; pComY[off + i] = 0.0; return; }
  let m = f32(mi);
  let cx = i % R; let cy = i / R;
  let gg = geom(bbox[0], bbox[1], bbox[2], bbox[3]); let cs = gg.z / f32(R);
  let ccx = gg.x + (f32(cx) + 0.5) * cs;
  let ccy = gg.y + (f32(cy) + 0.5) * cs;
  pMass[off + i] = m;
  pComX[off + i] = ccx + (f32(sSumX[i]) / FRC) / m;
  pComY[off + i] = ccy + (f32(sSumY[i]) / FRC) / m;
}`;

const REDUCE = /* wgsl */ `
struct Lvl { level: u32 };
@group(0) @binding(1) var<uniform> LV: Lvl;
@group(0) @binding(2) var<storage, read_write> pMass: array<f32>;
@group(0) @binding(3) var<storage, read_write> pComX: array<f32>;
@group(0) @binding(4) var<storage, read_write> pComY: array<f32>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; let res = 1u << LV.level; let cnt = res * res; if (i >= cnt) { return; }
  let cx = i % res; let cy = i / res;
  let off = levelOffset(LV.level); let coff = levelOffset(LV.level + 1u); let cres = res << 1u;
  var m = 0.0; var sx = 0.0; var sy = 0.0;
  for (var j = 0u; j < 2u; j = j + 1u) {
    for (var k = 0u; k < 2u; k = k + 1u) {
      let ci = coff + (cy * 2u + j) * cres + (cx * 2u + k);
      let cm = pMass[ci];
      m = m + cm; sx = sx + cm * pComX[ci]; sy = sy + cm * pComY[ci];
    }
  }
  pMass[off + i] = m;
  if (m > 0.0) { pComX[off + i] = sx / m; pComY[off + i] = sy / m; }
  else { pComX[off + i] = 0.0; pComY[off + i] = 0.0; }
}`;

const CHARGE_BH = /* wgsl */ `
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> bbox: array<i32>;
@group(0) @binding(4) var<storage, read> pMass: array<f32>;
@group(0) @binding(5) var<storage, read> pComX: array<f32>;
@group(0) @binding(6) var<storage, read> pComY: array<f32>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  let p = pos[i];
  let gg = geom(bbox[0], bbox[1], bbox[2], bbox[3]);
  let R = P.gridRes;
  let pcx = clamp(u32((p.x - gg.x) / gg.z * f32(R)), 0u, R - 1u);
  let pcy = clamp(u32((p.y - gg.y) / gg.z * f32(R)), 0u, R - 1u);
  var acc = vec2<f32>(0.0, 0.0);
  var stack: array<u32, 64>;
  stack[0] = 0u; // root: level 0, cell (0,0)
  var sp = 1u;
  loop {
    if (sp == 0u) { break; }
    sp = sp - 1u;
    let node = stack[sp];
    let lvl = node >> 28u; let cx = (node >> 14u) & 0x3fffu; let cy = node & 0x3fffu;
    let idx = levelOffset(lvl) + cy * (1u << lvl) + cx;
    let m = pMass[idx];
    if (m == 0.0) { continue; }
    if (lvl == P.maxLevel && cx == pcx && cy == pcy) { continue; } // own leaf (self + cellmates)
    let dx = pComX[idx] - p.x; let dy = pComY[idx] - p.y;
    var l = dx * dx + dy * dy;
    let cw = gg.z / f32(1u << lvl);
    if (lvl == P.maxLevel || cw * cw < P.theta2 * l) {
      if (l < P.distMin2) { l = sqrt(P.distMin2 * l); }
      if (l > 0.0) {
        let w = P.charge * m * P.alpha / l;
        acc.x = acc.x + dx * w; acc.y = acc.y + dy * w;
      }
    } else if (sp + 4u <= 64u) {
      let nl = lvl + 1u; let bx = cx << 1u; let by = cy << 1u;
      stack[sp] = (nl << 28u) | (bx << 14u) | by; sp = sp + 1u;
      stack[sp] = (nl << 28u) | ((bx + 1u) << 14u) | by; sp = sp + 1u;
      stack[sp] = (nl << 28u) | (bx << 14u) | (by + 1u); sp = sp + 1u;
      stack[sp] = (nl << 28u) | ((bx + 1u) << 14u) | (by + 1u); sp = sp + 1u;
    }
  }
  vel[i] = vel[i] + acc;
}`;

const LINK = /* wgsl */ `
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> edges: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read> deg: array<f32>;
@group(0) @binding(5) var<storage, read_write> lf: array<atomic<i32>>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let e = g.x + g.y * 16384u; if (e >= P.m) { return; }
  let a = edges[e].x; let b = edges[e].y;
  if (a >= P.n || b >= P.n) { return; }
  let da = deg[a]; let db = deg[b];
  let x = (pos[b].x + vel[b].x) - (pos[a].x + vel[a].x);
  let y = (pos[b].y + vel[b].y) - (pos[a].y + vel[a].y);
  let len = sqrt(x * x + y * y);
  if (len == 0.0) { return; }
  let st = P.linkStrength / min(da, db); // d3 default 1/min(deg)
  let l = (len - P.linkDist) / len * P.alpha * st;
  let fx = x * l; let fy = y * l;
  let bias = da / (da + db);
  atomicAdd(&lf[2u * b], fp(-fx * bias));
  atomicAdd(&lf[2u * b + 1u], fp(-fy * bias));
  atomicAdd(&lf[2u * a], fp(fx * (1.0 - bias)));
  atomicAdd(&lf[2u * a + 1u], fp(fy * (1.0 - bias)));
}`;

const INTEGRATE = /* wgsl */ `
@group(0) @binding(1) var<storage, read_write> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read_write> vel: array<vec2<f32>>;
@group(0) @binding(3) var<storage, read> lf: array<i32>;
@compute @workgroup_size(${WG}) fn main(@builtin(global_invocation_id) g: vec3<u32>) {
  let i = g.x + g.y * 16384u; if (i >= P.n) { return; }
  let p = pos[i];
  var v = vel[i];
  v = v + vec2<f32>(f32(lf[2u * i]), f32(lf[2u * i + 1u])) / FRC;
  v = v + (vec2<f32>(0.0, 0.0) - p) * (P.center * P.alpha);
  v = v * P.velDecay;
  vel[i] = v;
  pos[i] = p + v;
}`;

export type ChargeMode = "auto" | "exact" | "bh";

export interface GpuLayoutOptions {
  device: GPUDevice;
  nodeCount: number;
  edges: Uint32Array; // flat [src,dst,...]
  positions?: Float32Array;
  params?: Partial<GpuParams>;
  alpha?: number;
  alphaDecay?: number;
  alphaMin?: number;
  /** Charge backend: "auto" (Barnes-Hut above bhThreshold), or force "exact"/"bh". */
  chargeMode?: ChargeMode;
  /** Node count above which "auto" uses Barnes-Hut (default 4096). */
  bhThreshold?: number;
}

interface Pass {
  pipeline: GPUComputePipeline;
  bind: GPUBindGroup;
}

export class GpuLayout {
  readonly device: GPUDevice;
  readonly n: number;
  readonly m: number;
  readonly bh: boolean;
  readonly maxLevel: number;
  readonly gridRes: number;
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
  // Barnes-Hut buffers (allocated only when bh)
  private bboxBuf?: GPUBuffer;
  private sMassBuf?: GPUBuffer;
  private sSumXBuf?: GPUBuffer;
  private sSumYBuf?: GPUBuffer;
  private pMassBuf?: GPUBuffer;
  private pComXBuf?: GPUBuffer;
  private pComYBuf?: GPUBuffer;
  private reduceLvlBuf?: GPUBuffer;

  private readonly pass: Record<string, Pass> = {};
  private readonly buffers: GPUBuffer[] = [];

  constructor(opts: GpuLayoutOptions) {
    const { device } = opts;
    this.device = device;
    this.n = opts.nodeCount;
    this.m = opts.edges.length >>> 1;
    this.p = { ...DEFAULT_PARAMS, ...opts.params };
    this.alpha = opts.alpha ?? 1;
    this.alphaDecay = opts.alphaDecay ?? 1 - Math.pow(0.001, 1 / 300);
    this.alphaMin = opts.alphaMin ?? 0.001;

    const mode = opts.chargeMode ?? "auto";
    const threshold = opts.bhThreshold ?? 4096;
    this.bh = mode === "bh" || (mode === "auto" && this.n > threshold);
    // Finest grid ~ a few bodies/cell: floor(log2(sqrt(n))) keeps the tree passes
    // (O(cells)) balanced against the node passes (O(n)) instead of over-refining.
    this.maxLevel = Math.min(11, Math.max(4, Math.floor(Math.log2(Math.max(2, Math.sqrt(this.n))))));
    this.gridRes = 1 << this.maxLevel;

    const ST = GPUBufferUsage.STORAGE;
    const CD = GPUBufferUsage.COPY_DST;
    const track = (b: GPUBuffer) => (this.buffers.push(b), b);
    const mk = (bytes: number, usage: number) => track(device.createBuffer({ size: Math.max(16, bytes), usage }));

    this.posBuf = mk(this.n * 8, ST | CD | GPUBufferUsage.COPY_SRC);
    this.velBuf = mk(this.n * 8, ST | CD);
    this.edgeBuf = mk(this.m * 8, ST | CD);
    this.lfBuf = mk(this.n * 8, ST | CD);
    this.degBuf = mk(this.n * 4, ST | CD);
    this.paramBuf = track(device.createBuffer({ size: 48, usage: GPUBufferUsage.UNIFORM | CD }));

    const pos = opts.positions ?? phyllotaxis(this.n, this.p.linkDistance);
    device.queue.writeBuffer(this.posBuf, 0, pos as BufferSource);
    device.queue.writeBuffer(this.velBuf, 0, new Float32Array(this.n * 2));
    if (this.m > 0) device.queue.writeBuffer(this.edgeBuf, 0, opts.edges as BufferSource);
    const deg = new Float32Array(this.n);
    for (let i = 0; i < this.m; i++) { deg[opts.edges[2 * i]]++; deg[opts.edges[2 * i + 1]]++; }
    device.queue.writeBuffer(this.degBuf, 0, deg);

    // Shared bits used by several passes.
    this.makePass("clearForce", CLEAR_FORCE, [this.paramBuf, [this.lfBuf, "rw"]]);
    this.makePass("link", LINK, [
      this.paramBuf, [this.posBuf, "ro"], [this.velBuf, "ro"], [this.edgeBuf, "ro"], [this.degBuf, "ro"], [this.lfBuf, "rw"],
    ]);
    this.makePass("integrate", INTEGRATE, [this.paramBuf, [this.posBuf, "rw"], [this.velBuf, "rw"], [this.lfBuf, "ro"]]);

    if (this.bh) {
      const finest = this.gridRes * this.gridRes;
      let total = 0;
      for (let l = 0; l <= this.maxLevel; l++) total += 1 << (2 * l);
      this.bboxBuf = mk(16, ST | CD);
      this.sMassBuf = mk(finest * 4, ST);
      this.sSumXBuf = mk(finest * 4, ST);
      this.sSumYBuf = mk(finest * 4, ST);
      this.pMassBuf = mk(total * 4, ST);
      this.pComXBuf = mk(total * 4, ST);
      this.pComYBuf = mk(total * 4, ST);
      // Per-level uniform for the reduce pass, addressed by dynamic offset.
      this.reduceLvlBuf = track(device.createBuffer({ size: 256 * this.maxLevel, usage: GPUBufferUsage.UNIFORM | CD }));
      const lv = new Uint32Array(64 * this.maxLevel);
      for (let l = 0; l < this.maxLevel; l++) lv[l * 64] = l;
      device.queue.writeBuffer(this.reduceLvlBuf, 0, lv);

      this.makePass("clearBbox", CLEAR_BBOX, [this.paramBuf, [this.bboxBuf, "rw"]]);
      this.makePass("bbox", BBOX, [this.paramBuf, [this.posBuf, "ro"], [this.bboxBuf, "rw"]]);
      this.makePass("clearScatter", CLEAR_SCATTER, [this.paramBuf, [this.sMassBuf, "rw"], [this.sSumXBuf, "rw"], [this.sSumYBuf, "rw"]]);
      this.makePass("scatter", SCATTER, [this.paramBuf, [this.posBuf, "ro"], [this.bboxBuf, "ro"], [this.sMassBuf, "rw"], [this.sSumXBuf, "rw"], [this.sSumYBuf, "rw"]]);
      this.makePass("finalize", FINALIZE, [
        this.paramBuf, [this.bboxBuf, "ro"], [this.sMassBuf, "ro"], [this.sSumXBuf, "ro"], [this.sSumYBuf, "ro"],
        [this.pMassBuf, "rw"], [this.pComXBuf, "rw"], [this.pComYBuf, "rw"],
      ]);
      this.makePass("reduce", REDUCE, [this.paramBuf, [this.reduceLvlBuf, "dyn"], [this.pMassBuf, "rw"], [this.pComXBuf, "rw"], [this.pComYBuf, "rw"]]);
      this.makePass("chargeBH", CHARGE_BH, [this.paramBuf, [this.posBuf, "ro"], [this.velBuf, "rw"], [this.bboxBuf, "ro"], [this.pMassBuf, "ro"], [this.pComXBuf, "ro"], [this.pComYBuf, "ro"]]);
    } else {
      this.makePass("charge", CHARGE_EXACT, [this.paramBuf, [this.posBuf, "ro"], [this.velBuf, "rw"]]);
    }

    this.writeParams();
  }

  private makePass(name: string, body: string, bufs: (GPUBuffer | [GPUBuffer, "ro" | "rw" | "dyn"])[]) {
    const device = this.device;
    const entries: GPUBindGroupLayoutEntry[] = bufs.map((b, i) => {
      if (i === 0) return { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: "uniform" } };
      const kind = Array.isArray(b) ? b[1] : "rw";
      if (kind === "dyn") return { binding: i, visibility: GPUShaderStage.COMPUTE, buffer: { type: "uniform", hasDynamicOffset: true } };
      return { binding: i, visibility: GPUShaderStage.COMPUTE, buffer: { type: kind === "ro" ? "read-only-storage" : "storage" } };
    });
    const layout = device.createBindGroupLayout({ entries });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    const module = device.createShaderModule({ code: PRELUDE + body });
    const pipeline = device.createComputePipeline({ layout: pl, compute: { module, entryPoint: "main" } });
    const bind = device.createBindGroup({
      layout,
      entries: bufs.map((b, i) => {
        const buf = Array.isArray(b) ? b[0] : b;
        const kind = Array.isArray(b) ? b[1] : undefined;
        return { binding: i, resource: kind === "dyn" ? { buffer: buf, size: 16 } : { buffer: buf } };
      }),
    });
    this.pass[name] = { pipeline, bind };
  }

  reheat(alpha = 0.6) { this.alpha = Math.max(this.alpha, alpha); }
  get settled(): boolean { return this.alpha < this.alphaMin; }
  get positions(): GPUBuffer { return this.posBuf; }
  get edgeBuffer(): GPUBuffer { return this.edgeBuf; }
  pin(index: number, x: number, y: number) {
    this.device.queue.writeBuffer(this.posBuf, index * 8, new Float32Array([x, y]));
    this.device.queue.writeBuffer(this.velBuf, index * 8, new Float32Array([0, 0]));
  }

  private writeParams() {
    const ab = new ArrayBuffer(48);
    const u = new Uint32Array(ab);
    const f = new Float32Array(ab);
    u[0] = this.n; u[1] = this.m; u[2] = this.maxLevel; u[3] = this.gridRes;
    f[4] = this.p.charge; f[5] = this.p.linkDistance; f[6] = this.p.linkStrength; f[7] = this.p.center;
    f[8] = this.p.velDecay; f[9] = this.p.distanceMin2; f[10] = this.alpha; f[11] = this.p.theta * this.p.theta;
    this.device.queue.writeBuffer(this.paramBuf, 0, ab);
  }

  step(iterations = 1) {
    if (this.alpha < this.alphaMin) return;
    const enc = this.device.createCommandEncoder();
    for (let it = 0; it < iterations; it++) {
      this.alpha += (0 - this.alpha) * this.alphaDecay;
      this.writeParams();
      const pass = enc.beginComputePass();
      const run = (name: string, count: number, dyn?: number) => {
        const p = this.pass[name];
        pass.setPipeline(p.pipeline);
        if (dyn !== undefined) pass.setBindGroup(0, p.bind, [dyn]);
        else pass.setBindGroup(0, p.bind);
        // 2D dispatch: x is fixed at GX=256 workgroups (256*WG = 16384 invocations
        // wide, the shader's linear-index stride); y covers the rest. Keeps each
        // dimension under the 65535 workgroup limit so it scales past ~4M cells.
        const GX = 256;
        const gy = Math.max(1, Math.ceil(count / (GX * WG)));
        pass.dispatchWorkgroups(GX, gy);
      };
      run("clearForce", this.n);
      if (this.bh) {
        const finest = this.gridRes * this.gridRes;
        run("clearBbox", 4);
        run("bbox", this.n);
        run("clearScatter", finest);
        run("scatter", this.n);
        run("finalize", finest);
        for (let lvl = this.maxLevel - 1; lvl >= 0; lvl--) run("reduce", 1 << (2 * lvl), lvl * 256);
        run("chargeBH", this.n);
      } else {
        run("charge", this.n);
      }
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
    for (const b of this.buffers) b.destroy();
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
