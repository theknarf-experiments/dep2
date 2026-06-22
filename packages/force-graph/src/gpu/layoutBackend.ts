/// <reference types="@webgpu/types" />
// A drop-in replacement for the d3-force Web Worker (forceWorker.ts): it speaks
// the exact same message protocol (set / drag / dragEnd in, tick out), so
// ForceGraph can run its layout on the GPU without changing any of its rendering
// or interaction code. The layout itself is GpuLayout, our verified-exact port
// of d3-force (see gpu/sim.ts + test/gpu-oracle.ts). Rendering stays on R3F /
// WebGL — only the force simulation moves to the GPU, which is the part that was
// the bottleneck.

import { GpuLayout } from "./sim";

interface SetMsg {
  type: "set";
  version: number;
  nodes: { id: string; x: number; y: number; r: number }[];
  links: { source: string; target: string }[];
  alpha: number;
}
interface DragMsg { type: "drag"; id: string; x: number; y: number }
interface DragEndMsg { type: "dragEnd"; id: string }
type InMsg = SetMsg | DragMsg | DragEndMsg;

export interface TickMsg { type: "tick"; version: number; pos: Float32Array }

/** The subset of Worker that ForceGraph uses, so either backend slots in. */
export interface LayoutBackend {
  onmessage: ((e: { data: TickMsg }) => void) | null;
  postMessage(msg: InMsg): void;
  terminate(): void;
}

export function gpuLayoutSupported(): boolean {
  return typeof navigator !== "undefined" && !!(navigator as Navigator).gpu;
}

export class GpuLayoutBackend implements LayoutBackend {
  onmessage: ((e: { data: TickMsg }) => void) | null = null;

  private device: GPUDevice | null = null;
  private sim: GpuLayout | null = null;
  private index = new Map<string, number>();
  private version = 0;
  private dragIdx = -1;
  private dragX = 0;
  private dragY = 0;
  private raf = 0;
  private disposed = false;
  private reading = false;
  private pendingSet: SetMsg | null = null;
  private readonly onError: (reason: string) => void;

  constructor(onError: (reason: string) => void) {
    this.onError = onError;
    void this.init();
  }

  private async init() {
    try {
      const adapter = await navigator.gpu?.requestAdapter();
      const device = await adapter?.requestDevice();
      if (!device) throw new Error("WebGPU: no adapter/device");
      if (this.disposed) return;
      void device.lost.then((info) => {
        if (!this.disposed) this.onError(`WebGPU device lost: ${info.message}`);
      });
      this.device = device;
      if (this.pendingSet) {
        this.applySet(this.pendingSet);
        this.pendingSet = null;
      }
      this.raf = requestAnimationFrame(this.loop);
    } catch (e) {
      if (!this.disposed) this.onError(String(e));
    }
  }

  postMessage(msg: InMsg) {
    if (msg.type === "set") {
      if (!this.device) this.pendingSet = msg; // applied once the device is ready
      else this.applySet(msg);
    } else if (msg.type === "drag") {
      const i = this.index.get(msg.id);
      if (i === undefined) return;
      this.dragIdx = i;
      this.dragX = msg.x;
      this.dragY = msg.y;
      this.sim?.reheat(0.3); // d3 worker uses alphaTarget(0.3) while dragging
    } else if (msg.type === "dragEnd") {
      this.dragIdx = -1; // let it cool back down naturally (alphaTarget 0)
    }
  }

  private applySet(msg: SetMsg) {
    const n = msg.nodes.length;
    this.index = new Map(msg.nodes.map((nd, i) => [nd.id, i]));
    const pos = new Float32Array(n * 2);
    msg.nodes.forEach((nd, i) => { pos[2 * i] = nd.x; pos[2 * i + 1] = nd.y; });
    const edges: number[] = [];
    for (const l of msg.links) {
      const s = this.index.get(l.source);
      const t = this.index.get(l.target);
      if (s !== undefined && t !== undefined) edges.push(s, t);
    }
    this.version = msg.version;
    this.dragIdx = -1;
    this.sim?.destroy();
    this.sim = new GpuLayout({
      device: this.device!,
      nodeCount: n,
      edges: new Uint32Array(edges),
      positions: pos,
      // alpha 0 (everything already placed) => static; otherwise animate.
      alpha: msg.alpha > 0 ? msg.alpha : 0,
    });
    this.postTick(pos); // show the seeded positions immediately
  }

  private loop = () => {
    if (this.disposed) return;
    const sim = this.sim;
    if (sim && !sim.settled && !this.reading) {
      sim.step(1);
      if (this.dragIdx >= 0) sim.pin(this.dragIdx, this.dragX, this.dragY); // hold dragged node
      this.reading = true;
      sim.readPositions()
        .then((p) => {
          this.reading = false;
          if (!this.disposed) this.postTick(p);
        })
        .catch((e) => {
          this.reading = false;
          if (!this.disposed) this.onError(String(e));
        });
    }
    this.raf = requestAnimationFrame(this.loop);
  };

  private postTick(pos: Float32Array) {
    this.onmessage?.({ data: { type: "tick", version: this.version, pos } });
  }

  terminate() {
    this.disposed = true;
    cancelAnimationFrame(this.raf);
    this.sim?.destroy();
    this.device?.destroy();
  }
}
