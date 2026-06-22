/// <reference types="@webgpu/types" />
// WebGPU renderer for the GPU force layout. Draws straight from the simulation's
// position buffer (no CPU round-trip): edges as lines, nodes as instanced circle
// quads, plus an integer "pick" pass that renders node indices to a texture so a
// single texel read identifies the node under the cursor. Device-agnostic — it
// renders into whatever texture view it's given (a canvas in the browser, an
// offscreen texture in headless tests).

export interface Camera {
  zoom: number; // pixels per world unit
  cx: number; // world-space center
  cy: number;
}
export interface Highlight {
  hovered: number; // node index or -1
  selected: number; // node index or -1
  activeGroup: number; // group id to spotlight, or -1 for none
}

const SHADER = /* wgsl */ `
struct Cam {
  zoom: f32, cx: f32, cy: f32, halfW: f32,
  halfH: f32, nodeScale: f32, hovered: i32, selected: i32,
  activeGroup: u32, hasActive: u32, _p0: u32, _p1: u32,
};
@group(0) @binding(0) var<uniform> cam: Cam;
@group(0) @binding(1) var<storage, read> pos: array<vec2<f32>>;
@group(0) @binding(2) var<storage, read> col: array<u32>;   // packed rgba8 (0xAABBGGRR)
@group(0) @binding(3) var<storage, read> rad: array<f32>;
@group(0) @binding(4) var<storage, read> grp: array<u32>;
@group(0) @binding(5) var<storage, read> edges: array<vec2<u32>>;

fn toClip(p: vec2<f32>) -> vec2<f32> {
  return vec2<f32>((p.x - cam.cx) * cam.zoom / cam.halfW, (p.y - cam.cy) * cam.zoom / cam.halfH);
}
fn unpack(c: u32) -> vec3<f32> {
  return vec3<f32>(f32(c & 0xffu), f32((c >> 8u) & 0xffu), f32((c >> 16u) & 0xffu)) / 255.0;
}
fn dimf(i: u32) -> f32 {
  if (cam.hasActive == 1u && grp[i] != cam.activeGroup) { return 0.16; }
  return 1.0;
}
fn corner(vi: u32) -> vec2<f32> {
  var c = array<vec2<f32>, 6>(
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
  );
  return c[vi];
}
fn nodeRadius(i: u32) -> f32 {
  var r = rad[i] * cam.nodeScale;
  if (i32(i) == cam.selected) { r = r * 1.6; }
  return r;
}

// ---- nodes ----
struct VOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec3<f32> };
@vertex fn vnode(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VOut {
  let q = corner(vi);
  let world = pos[ii] + q * nodeRadius(ii);
  var o: VOut;
  o.clip = vec4<f32>(toClip(world), 0.0, 1.0);
  o.uv = q;
  var c = unpack(col[ii]) * dimf(ii);
  if (i32(ii) == cam.hovered || i32(ii) == cam.selected) { c = mix(c, vec3<f32>(1.0, 1.0, 1.0), 0.35); }
  o.color = c;
  return o;
}
@fragment fn fnode(in: VOut) -> @location(0) vec4<f32> {
  if (length(in.uv) > 1.0) { discard; }
  return vec4<f32>(in.color, 1.0);
}

// ---- edges ----
struct EOut { @builtin(position) clip: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vedge(@builtin(vertex_index) vi: u32) -> EOut {
  let e = vi / 2u;
  let pr = edges[e];
  let idx = select(pr.x, pr.y, (vi & 1u) == 1u);
  var o: EOut;
  o.clip = vec4<f32>(toClip(pos[idx]), 0.0, 1.0);
  let d = min(dimf(pr.x), dimf(pr.y));
  o.color = vec4<f32>(unpack(col[pr.y]) * 0.55 * d, 0.55 * d);
  return o;
}
@fragment fn fedge(in: EOut) -> @location(0) vec4<f32> { return in.color; }

// ---- pick (instance index -> R32Uint) ----
struct POut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) @interpolate(flat) id: u32 };
@vertex fn vpick(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> POut {
  let q = corner(vi);
  let world = pos[ii] + q * nodeRadius(ii);
  var o: POut;
  o.clip = vec4<f32>(toClip(world), 0.0, 1.0);
  o.uv = q;
  o.id = ii;
  return o;
}
@fragment fn fpick(in: POut) -> @location(0) u32 {
  if (length(in.uv) > 1.0) { discard; }
  return in.id + 1u; // 0 = background
}
`;

export interface GpuGraph {
  n: number;
  posBuffer: GPUBuffer; // from GpuLayout.positions
  edgeBuffer: GPUBuffer; // from GpuLayout.edgeBuffer
  edgeCount: number;
  colors: Uint32Array; // packed 0xAABBGGRR per node
  radii: Float32Array; // per node
  groups: Uint32Array; // group id per node
}

export class GpuRenderer {
  private readonly device: GPUDevice;
  private readonly camBuf: GPUBuffer;
  private readonly nodePipe: GPURenderPipeline;
  private readonly edgePipe: GPURenderPipeline;
  private readonly pickPipe: GPURenderPipeline;
  private readonly layout: GPUBindGroupLayout;

  private g: GpuGraph | null = null;
  private colBuf?: GPUBuffer;
  private radBuf?: GPUBuffer;
  private grpBuf?: GPUBuffer;
  private bind?: GPUBindGroup;
  private pickTex?: GPUTexture;
  private pickW = 0;
  private pickH = 0;

  constructor(device: GPUDevice, format: GPUTextureFormat) {
    this.device = device;
    this.camBuf = device.createBuffer({ size: 48, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
    const mod = device.createShaderModule({ code: SHADER });
    this.layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT, buffer: { type: "uniform" } },
        ...[1, 2, 3, 4, 5].map((binding) => ({
          binding,
          visibility: GPUShaderStage.VERTEX | GPUShaderStage.FRAGMENT,
          buffer: { type: "read-only-storage" as const },
        })),
      ],
    });
    const pl = device.createPipelineLayout({ bindGroupLayouts: [this.layout] });
    const blend: GPUBlendState = {
      color: { srcFactor: "src-alpha", dstFactor: "one-minus-src-alpha", operation: "add" },
      alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha", operation: "add" },
    };
    this.edgePipe = device.createRenderPipeline({
      layout: pl,
      vertex: { module: mod, entryPoint: "vedge" },
      fragment: { module: mod, entryPoint: "fedge", targets: [{ format, blend }] },
      primitive: { topology: "line-list" },
    });
    this.nodePipe = device.createRenderPipeline({
      layout: pl,
      vertex: { module: mod, entryPoint: "vnode" },
      fragment: { module: mod, entryPoint: "fnode", targets: [{ format, blend }] },
      primitive: { topology: "triangle-list" },
    });
    this.pickPipe = device.createRenderPipeline({
      layout: pl,
      vertex: { module: mod, entryPoint: "vpick" },
      fragment: { module: mod, entryPoint: "fpick", targets: [{ format: "r32uint" }] },
      primitive: { topology: "triangle-list" },
    });
  }

  setGraph(g: GpuGraph) {
    this.colBuf?.destroy();
    this.radBuf?.destroy();
    this.grpBuf?.destroy();
    const dev = this.device;
    const mk = (data: Uint32Array | Float32Array) => {
      const b = dev.createBuffer({ size: Math.max(16, data.byteLength), usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST });
      dev.queue.writeBuffer(b, 0, data as BufferSource);
      return b;
    };
    this.colBuf = mk(g.colors);
    this.radBuf = mk(g.radii);
    this.grpBuf = mk(g.groups);
    this.g = g;
    this.bind = dev.createBindGroup({
      layout: this.layout,
      entries: [
        { binding: 0, resource: { buffer: this.camBuf } },
        { binding: 1, resource: { buffer: g.posBuffer } },
        { binding: 2, resource: { buffer: this.colBuf } },
        { binding: 3, resource: { buffer: this.radBuf } },
        { binding: 4, resource: { buffer: this.grpBuf } },
        { binding: 5, resource: { buffer: g.edgeBuffer } },
      ],
    });
  }

  private writeCam(width: number, height: number, cam: Camera, hi: Highlight) {
    const ab = new ArrayBuffer(48);
    const f = new Float32Array(ab);
    const i = new Int32Array(ab);
    const u = new Uint32Array(ab);
    f[0] = cam.zoom;
    f[1] = cam.cx;
    f[2] = cam.cy;
    f[3] = width / 2;
    f[4] = height / 2;
    f[5] = 1;
    i[6] = hi.hovered;
    i[7] = hi.selected;
    u[8] = hi.activeGroup < 0 ? 0 : hi.activeGroup;
    u[9] = hi.activeGroup < 0 ? 0 : 1;
    this.device.queue.writeBuffer(this.camBuf, 0, ab);
  }

  /** Draw edges + nodes into `view` (size in physical pixels). */
  draw(view: GPUTextureView, width: number, height: number, cam: Camera, hi: Highlight) {
    if (!this.g || !this.bind) return;
    this.writeCam(width, height, cam, hi);
    const enc = this.device.createCommandEncoder();
    const pass = enc.beginRenderPass({
      colorAttachments: [{ view, clearValue: { r: 0.055, g: 0.055, b: 0.067, a: 1 }, loadOp: "clear", storeOp: "store" }],
    });
    pass.setBindGroup(0, this.bind);
    if (this.g.edgeCount > 0) {
      pass.setPipeline(this.edgePipe);
      pass.draw(this.g.edgeCount * 2);
    }
    pass.setPipeline(this.nodePipe);
    pass.draw(6, this.g.n);
    pass.end();
    this.device.queue.submit([enc.finish()]);
  }

  /** Return the node index under physical pixel (px,py), or -1. */
  async pick(px: number, py: number, width: number, height: number, cam: Camera, hi: Highlight): Promise<number> {
    if (!this.g || !this.bind) return -1;
    if (px < 0 || py < 0 || px >= width || py >= height) return -1;
    if (!this.pickTex || this.pickW !== width || this.pickH !== height) {
      this.pickTex?.destroy();
      this.pickTex = this.device.createTexture({
        size: { width, height },
        format: "r32uint",
        usage: GPUTextureUsage.RENDER_ATTACHMENT | GPUTextureUsage.COPY_SRC,
      });
      this.pickW = width;
      this.pickH = height;
    }
    this.writeCam(width, height, cam, hi);
    const enc = this.device.createCommandEncoder();
    const pass = enc.beginRenderPass({
      colorAttachments: [{ view: this.pickTex.createView(), clearValue: { r: 0, g: 0, b: 0, a: 0 }, loadOp: "clear", storeOp: "store" }],
    });
    pass.setBindGroup(0, this.bind);
    pass.setPipeline(this.pickPipe);
    pass.draw(6, this.g.n);
    pass.end();
    // copy the single texel under the cursor (256-byte row alignment).
    const staging = this.device.createBuffer({ size: 256, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
    enc.copyTextureToBuffer(
      { texture: this.pickTex, origin: { x: Math.floor(px), y: Math.floor(py) } },
      { buffer: staging, bytesPerRow: 256 },
      { width: 1, height: 1 },
    );
    this.device.queue.submit([enc.finish()]);
    await staging.mapAsync(GPUMapMode.READ);
    const id = new Uint32Array(staging.getMappedRange())[0];
    staging.unmap();
    staging.destroy();
    return id === 0 ? -1 : id - 1;
  }

  destroy() {
    this.colBuf?.destroy();
    this.radBuf?.destroy();
    this.grpBuf?.destroy();
    this.pickTex?.destroy();
    this.camBuf.destroy();
  }
}

/** Parse a CSS color to packed 0xAABBGGRR. Browser only (uses a 2D canvas);
 *  tests pass numeric colors directly. */
export function packCssColor(css: string, cache: Map<string, number>, ctx?: CanvasRenderingContext2D): number {
  const hit = cache.get(css);
  if (hit !== undefined) return hit;
  let r = 136, g = 136, b = 150;
  if (ctx) {
    ctx.fillStyle = "#000";
    ctx.fillStyle = css;
    ctx.fillRect(0, 0, 1, 1);
    const d = ctx.getImageData(0, 0, 1, 1).data;
    r = d[0];
    g = d[1];
    b = d[2];
  }
  const packed = (255 << 24) | (b << 16) | (g << 8) | r;
  cache.set(css, packed >>> 0);
  return packed >>> 0;
}
