/// <reference types="@webgpu/types" />
// Self-contained WebGPU graph: owns a <canvas>, runs the GPU force sim
// (GpuLayout) and renders straight from its position buffer (GpuRenderer). Same
// hover/select/drag/pan/zoom interactions and prop shape as <ForceGraph>, so the
// app can swap to it. Falls back (via `onUnsupported`) when WebGPU is missing.

import { MutableRefObject, useEffect, useRef } from "react";
import { GraphElements, Perf } from "./types";
import { DEFAULT_RADIUS } from "./types";
import { GpuLayout } from "./gpu/sim";
import { Camera, GpuRenderer, packCssColor } from "./gpu/render";

export interface GpuForceGraphProps {
  elements: GraphElements;
  layoutKey?: string | number;
  hovered: string | null;
  setHovered: (id: string | null) => void;
  selected: string | null;
  setSelected: (id: string | null) => void;
  activeGroup?: string | null;
  perf?: MutableRefObject<Perf>;
  /** Called once if WebGPU can't be used (absent, or init failed) — fall back. */
  onUnsupported?: (reason: string) => void;
}

export function GpuForceGraph(props: GpuForceGraphProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  // Latest props for the rAF loop / event handlers (avoid re-subscribing).
  const p = useRef(props);
  p.current = props;

  useEffect(() => {
    const canvas = canvasRef.current!;
    let disposed = false;
    let raf = 0;
    let cleanupEvents = () => {};
    let cleanup = () => {};

    (async () => {
      const gpu = (navigator as Navigator & { gpu?: GPU }).gpu;
      if (!gpu) return p.current.onUnsupported?.("navigator.gpu unavailable");
      let adapter: GPUAdapter | null = null;
      try {
        adapter = await gpu.requestAdapter({ powerPreference: "high-performance" });
      } catch (e) {
        return p.current.onUnsupported?.(String(e));
      }
      if (!adapter) return p.current.onUnsupported?.("no GPU adapter");
      const device = await adapter.requestDevice();
      if (disposed) return;

      const ctx = canvas.getContext("webgpu") as GPUCanvasContext | null;
      if (!ctx) return p.current.onUnsupported?.("no webgpu canvas context");
      const format = gpu.getPreferredCanvasFormat();
      ctx.configure({ device, format, alphaMode: "opaque" });
      device.addEventListener?.("uncapturederror", (e) =>
        console.error("[GpuForceGraph] device error:", (e as GPUUncapturedErrorEvent).error),
      );

      const renderer = new GpuRenderer(device, format);

      // ---- build graph state (ids <-> indices, packed colors, groups) ----
      const colorCanvas = document.createElement("canvas");
      colorCanvas.width = colorCanvas.height = 1;
      const colorCtx = colorCanvas.getContext("2d", { willReadFrequently: true }) ?? undefined;
      const colorCache = new Map<string, number>();

      let order: string[] = [];
      let idIndex = new Map<string, number>();
      let groupId = new Map<string, number>();
      let sim: GpuLayout | null = null;
      let dragIndex = -1;
      let fitPending = true;

      const cam: Camera = { zoom: 1, cx: 0, cy: 0 };
      const fpsState = { frames: 0, t: 0, worst: 0, last: performance.now() };

      function build(elements: GraphElements) {
        const nodes = elements.nodes;
        order = nodes.map((n) => n.id);
        idIndex = new Map(order.map((id, i) => [id, i]));
        groupId = new Map();
        const groups = new Uint32Array(nodes.length);
        const colors = new Uint32Array(nodes.length);
        const radii = new Float32Array(nodes.length);
        nodes.forEach((n, i) => {
          const g = n.group ?? "";
          let gid = groupId.get(g);
          if (gid === undefined) {
            gid = groupId.size;
            groupId.set(g, gid);
          }
          groups[i] = gid;
          colors[i] = packCssColor(n.color, colorCache, colorCtx);
          radii[i] = n.radius ?? DEFAULT_RADIUS;
        });
        const epairs: number[] = [];
        for (const e of elements.edges) {
          const s = idIndex.get(e.source);
          const t = idIndex.get(e.target);
          if (s !== undefined && t !== undefined) epairs.push(s, t);
        }
        const edges = new Uint32Array(epairs);
        sim?.destroy();
        sim = new GpuLayout({ device, nodeCount: nodes.length, edges });
        renderer.setGraph({
          n: nodes.length,
          posBuffer: sim.positions,
          edgeBuffer: sim.edgeBuffer,
          edgeCount: edges.length >>> 1,
          colors,
          radii,
          groups,
        });
        fitPending = true;
      }

      build(p.current.elements);
      let lastElements = p.current.elements;
      let lastLayoutKey = p.current.layoutKey;

      // ---- sizing ----
      function physicalSize(): [number, number] {
        const dpr = Math.min(window.devicePixelRatio || 1, 2);
        const w = Math.max(1, Math.round(canvas.clientWidth * dpr));
        const h = Math.max(1, Math.round(canvas.clientHeight * dpr));
        return [w, h];
      }
      function resize() {
        const [w, h] = physicalSize();
        if (canvas.width !== w || canvas.height !== h) {
          canvas.width = w;
          canvas.height = h;
        }
      }
      async function fitView() {
        if (!sim) return;
        const pos = await sim.readPositions();
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        for (let i = 0; i < sim.n; i++) {
          const x = pos[2 * i], y = pos[2 * i + 1];
          if (!Number.isFinite(x)) continue;
          minX = Math.min(minX, x); maxX = Math.max(maxX, x);
          minY = Math.min(minY, y); maxY = Math.max(maxY, y);
        }
        if (!Number.isFinite(minX)) return;
        const [w, h] = physicalSize();
        const spanX = Math.max(maxX - minX, 1), spanY = Math.max(maxY - minY, 1);
        cam.zoom = Math.min(w / (spanX * 1.2), h / (spanY * 1.2));
        cam.cx = (minX + maxX) / 2;
        cam.cy = (minY + maxY) / 2;
      }

      // ---- interaction ----
      const dpr = () => Math.min(window.devicePixelRatio || 1, 2);
      function toPhysical(clientX: number, clientY: number): [number, number] {
        const r = canvas.getBoundingClientRect();
        return [(clientX - r.left) * dpr(), (clientY - r.top) * dpr()];
      }
      function toWorld(clientX: number, clientY: number): [number, number] {
        const [px, py] = toPhysical(clientX, clientY);
        return [cam.cx + (px - canvas.width / 2) / cam.zoom, cam.cy - (py - canvas.height / 2) / cam.zoom];
      }
      function highlight() {
        const cur = p.current;
        const ag = cur.activeGroup != null ? groupId.get(cur.activeGroup) : undefined;
        return {
          hovered: cur.hovered != null ? idIndex.get(cur.hovered) ?? -1 : -1,
          selected: cur.selected != null ? idIndex.get(cur.selected) ?? -1 : -1,
          activeGroup: ag ?? -1,
        };
      }

      let userInteracted = false;
      let pickBusy = false;
      const pointers = new Map<number, { x: number; y: number }>();
      let panning = false;
      let moved = false;
      let last = { x: 0, y: 0 };
      let pinch = 0;

      const zoomAt = (clientX: number, clientY: number, factor: number) => {
        const [wx, wy] = toWorld(clientX, clientY);
        cam.zoom = Math.max(0.02, Math.min(cam.zoom * factor, 5000));
        const [wx2, wy2] = toWorld(clientX, clientY);
        cam.cx += wx - wx2;
        cam.cy += wy - wy2;
      };

      const onWheel = (e: WheelEvent) => {
        e.preventDefault();
        userInteracted = true;
        if (e.ctrlKey) zoomAt(e.clientX, e.clientY, Math.exp(-e.deltaY * 0.01));
        else {
          cam.cx += e.deltaX / cam.zoom;
          cam.cy += e.deltaY / cam.zoom;
        }
      };
      const onDown = async (e: PointerEvent) => {
        if (e.pointerType === "touch") {
          pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
          if (pointers.size >= 2) {
            const [a, b] = [...pointers.values()];
            pinch = Math.hypot(a.x - b.x, a.y - b.y);
            panning = false;
            dragIndex = -1;
            return;
          }
        }
        moved = false;
        userInteracted = true;
        const [px, py] = toPhysical(e.clientX, e.clientY);
        const hit = await pickAt(px, py);
        if (hit >= 0) dragIndex = hit;
        else {
          panning = true;
          last = { x: e.clientX, y: e.clientY };
        }
      };
      const onMove = async (e: PointerEvent) => {
        if (pointers.has(e.pointerId)) pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
        if (pointers.size >= 2) {
          const [a, b] = [...pointers.values()];
          const d = Math.hypot(a.x - b.x, a.y - b.y);
          if (pinch > 0) zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / pinch);
          pinch = d;
          return;
        }
        if (dragIndex >= 0) {
          moved = true;
          const [wx, wy] = toWorld(e.clientX, e.clientY);
          sim?.pin(dragIndex, wx, wy);
          sim?.reheat(0.3);
          return;
        }
        if (panning) {
          moved = true;
          cam.cx -= (e.clientX - last.x) / cam.zoom * dpr();
          cam.cy += (e.clientY - last.y) / cam.zoom * dpr();
          last = { x: e.clientX, y: e.clientY };
          return;
        }
        // hover pick (throttled: one in flight)
        if (pickBusy) return;
        pickBusy = true;
        const [px, py] = toPhysical(e.clientX, e.clientY);
        const idx = await pickAt(px, py);
        pickBusy = false;
        const id = idx >= 0 ? order[idx] : null;
        if (id !== p.current.hovered) p.current.setHovered(id);
      };
      const onUp = (e: PointerEvent) => {
        pointers.delete(e.pointerId);
        if (pointers.size < 2) pinch = 0;
        if (dragIndex >= 0) {
          if (!moved) p.current.setSelected(order[dragIndex]);
          dragIndex = -1;
        } else if (panning) {
          if (!moved) p.current.setSelected(null);
          panning = false;
        }
      };
      const onCancel = () => {
        pointers.clear();
        pinch = 0;
        dragIndex = -1;
        panning = false;
      };

      async function pickAt(px: number, py: number): Promise<number> {
        if (!sim) return -1;
        try {
          return await renderer.pick(px, py, canvas.width, canvas.height, cam, highlight());
        } catch {
          return -1;
        }
      }

      canvas.addEventListener("wheel", onWheel, { passive: false });
      canvas.addEventListener("pointerdown", onDown);
      window.addEventListener("pointermove", onMove);
      window.addEventListener("pointerup", onUp);
      window.addEventListener("pointercancel", onCancel);
      cleanupEvents = () => {
        canvas.removeEventListener("wheel", onWheel);
        canvas.removeEventListener("pointerdown", onDown);
        window.removeEventListener("pointermove", onMove);
        window.removeEventListener("pointerup", onUp);
        window.removeEventListener("pointercancel", onCancel);
      };

      // ---- main loop ----
      let fitCountdown = 90; // settle a bit, then fit once
      const frame = () => {
        if (disposed || !sim) return;
        try {
          tick();
        } catch (e) {
          // A GPU error mid-render shouldn't leave a frozen canvas — fall back.
          console.error("[GpuForceGraph] render error, falling back:", e);
          p.current.onUnsupported?.(String(e));
          return;
        }
        raf = requestAnimationFrame(frame);
      };
      const tick = () => {
        if (!sim) return;
        resize();

        // Rebuild on a new dataset; refit on a layoutKey change.
        if (p.current.elements !== lastElements) {
          lastElements = p.current.elements;
          build(p.current.elements);
          fitCountdown = 90;
        }
        if (p.current.layoutKey !== lastLayoutKey) {
          lastLayoutKey = p.current.layoutKey;
          if (!userInteracted) fitCountdown = 90;
        }

        const hot = sim.alpha > 0.025;
        if (hot) sim.step(1);

        if (fitPending && fitCountdown-- <= 0 && !userInteracted) {
          fitPending = false;
          void fitView();
        }

        renderer.draw(ctx.getCurrentTexture().createView(), canvas.width, canvas.height, cam, highlight());

        // fps
        const now = performance.now();
        const f = fpsState;
        const dt = (now - f.last) / 1000;
        f.last = now;
        f.frames++;
        f.t += dt;
        f.worst = Math.max(f.worst, dt);
        if (f.t >= 0.5 && p.current.perf) {
          p.current.perf.current.fps = Math.round(f.frames / f.t);
          p.current.perf.current.worstMs = f.worst * 1000;
          f.frames = 0;
          f.t = 0;
          f.worst = 0;
        }
      };
      raf = requestAnimationFrame(frame);

      cleanup = () => {
        cancelAnimationFrame(raf);
        cleanupEvents();
        sim?.destroy();
        renderer.destroy();
      };
    })().catch((e) => p.current.onUnsupported?.(String(e)));

    return () => {
      disposed = true;
      cancelAnimationFrame(raf);
      cleanupEvents();
      cleanup();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return <canvas ref={canvasRef} style={{ position: "absolute", inset: 0, width: "100%", height: "100%", display: "block", touchAction: "none" }} />;
}
