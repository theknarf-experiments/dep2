import { MutableRefObject, useEffect, useMemo, useRef, useState } from "react";
import { useFrame, useThree } from "@react-three/fiber";
import { OrthographicCamera, Text } from "@react-three/drei";
import * as THREE from "three";
import { DEFAULT_FONT_SIZE, DEFAULT_RADIUS, GraphElements, Perf } from "./types";

interface NodeMeta {
  id: string;
  label: string;
  group: string;
  color: THREE.Color;
  r: number;
  alwaysLabel: boolean;
  fontSize: number;
}

const MAX_NODES = 1 << 15; // 32768
const MAX_EDGES = 1 << 16; // 65536
const ARROW_MAX = 3000; // skip arrowheads above this (huge file graphs) for perf
const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

export interface ForceGraphProps {
  /** Nodes + edges to render. Re-passing reconciles in place (positions kept). */
  elements: GraphElements;
  /** Changing this opaque key re-fits the view (e.g. switching layouts/datasets). */
  layoutKey?: string | number;
  hovered: string | null;
  setHovered: (id: string | null) => void;
  selected: string | null;
  setSelected: (id: string | null) => void;
  /** Spotlight a group: nodes/edges outside `node.group === activeGroup` dim. */
  activeGroup?: string | null;
  /** Optional frame-timing readout, written ~twice a second. */
  perf?: MutableRefObject<Perf>;
}

export function ForceGraph({
  elements,
  layoutKey,
  hovered,
  setHovered,
  selected,
  setSelected,
  activeGroup = null,
  perf,
}: ForceGraphProps) {
  const { gl } = useThree();
  const get = useThree((s) => s.get);
  const camRef = useRef<THREE.OrthographicCamera>(null);

  const worker = useRef<Worker | null>(null);
  const order = useRef<string[]>([]); // node id by index
  const idIndex = useRef<Map<string, number>>(new Map());
  const meta = useRef<NodeMeta[]>([]); // index-aligned
  const pos = useRef<Float32Array>(new Float32Array(0)); // [x0,y0,x1,y1,...]
  const saved = useRef<Map<string, [number, number]>>(new Map()); // positions kept across reconciles
  const version = useRef(0);

  const linkSrc = useRef<Int32Array>(new Int32Array(MAX_EDGES));
  const linkDst = useRef<Int32Array>(new Int32Array(MAX_EDGES));
  const edgeCount = useRef(0);

  const nodesMesh = useRef<THREE.InstancedMesh>(null);
  const edgeGeom = useRef<THREE.BufferGeometry>(null);
  const arrowMesh = useRef<THREE.InstancedMesh>(null);
  const fitFrames = useRef(0);
  const userInteracted = useRef(false);
  const prevKey = useRef<string | number | undefined>(layoutKey);
  const dragIdx = useRef<number>(-1);
  // Only rebuild/upload instance + edge buffers when something actually changed
  // (a worker tick, a drag, a reconcile, or a hover/selection recolor); when the
  // layout is settled and idle we skip all of it and just let the GPU redraw.
  const dirty = useRef(true);

  const [nodeCount, setNodeCount] = useState(0);
  const [labels, setLabels] = useState<{ id: string; idx: number; label: string }[]>([]);

  const posArrBuf = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const colArrBuf = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const baseCol = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const dummy = useMemo(() => new THREE.Object3D(), []);
  const tmpColor = useMemo(() => new THREE.Color(), []);
  const raycaster = useMemo(() => new THREE.Raycaster(), []);
  const plane = useMemo(() => new THREE.Plane(new THREE.Vector3(0, 0, 1), 0), []);

  // Index set to keep lit (everything else dims): hovered node's neighborhood,
  // else the active module's members.
  const keepIdx = useMemo(() => {
    const m = idIndex.current;
    if (hovered) {
      const set = new Set<number>();
      const hi = m.get(hovered);
      if (hi !== undefined) set.add(hi);
      for (const e of elements.edges) {
        if (e.source === hovered) {
          const i = m.get(e.target);
          if (i !== undefined) set.add(i);
        }
        if (e.target === hovered) {
          const i = m.get(e.source);
          if (i !== undefined) set.add(i);
        }
      }
      return set;
    }
    if (activeGroup) {
      const set = new Set<number>();
      meta.current.forEach((n, i) => {
        if (n.group === activeGroup) set.add(i);
      });
      return set;
    }
    return null;
  }, [hovered, activeGroup, elements, nodeCount]);

  useEffect(() => {
    const w = new Worker(new URL("./forceWorker.ts", import.meta.url), { type: "module" });
    worker.current = w;
    w.onmessage = (e: MessageEvent) => {
      const m = e.data;
      if (m.type === "tick" && m.version === version.current) {
        pos.current = m.pos as Float32Array;
        dirty.current = true;
      }
    };
    return () => w.terminate();
  }, []);

  // Reconcile elements -> instanced graph.
  useEffect(() => {
    // Snapshot current positions (by id) so persistent nodes keep their place.
    const prevOrder = order.current;
    const p = pos.current;
    for (let i = 0; i < prevOrder.length; i++) {
      if (2 * i + 1 < p.length) saved.current.set(prevOrder[i], [p[2 * i], p[2 * i + 1]]);
    }

    const nodes = elements.nodes.slice(0, MAX_NODES);
    const ord: string[] = new Array(nodes.length);
    const ix = new Map<string, number>();
    const mlist: NodeMeta[] = new Array(nodes.length);
    const newPos = new Float32Array(nodes.length * 2);
    let allKnown = nodes.length > 0;
    nodes.forEach((n, i) => {
      ord[i] = n.id;
      ix.set(n.id, i);
      mlist[i] = {
        id: n.id,
        label: n.label,
        group: n.group ?? "",
        color: new THREE.Color(n.color),
        r: n.radius ?? DEFAULT_RADIUS,
        alwaysLabel: n.alwaysLabel ?? false,
        fontSize: n.fontSize ?? DEFAULT_FONT_SIZE,
      };
      const s = saved.current.get(n.id);
      if (s) {
        newPos[2 * i] = s[0];
        newPos[2 * i + 1] = s[1];
      } else {
        allKnown = false;
        newPos[2 * i] = (Math.random() - 0.5) * 80;
        newPos[2 * i + 1] = (Math.random() - 0.5) * 80;
      }
    });
    order.current = ord;
    idIndex.current = ix;
    meta.current = mlist;
    pos.current = newPos;

    // Links as index pairs; bake the source->target color gradient.
    const present = ix;
    let ec = 0;
    elements.edges.forEach((e) => {
      const s = present.get(e.source);
      const t = present.get(e.target);
      if (s === undefined || t === undefined || ec >= MAX_EDGES) return;
      linkSrc.current[ec] = s;
      linkDst.current[ec] = t;
      tmpColor.copy(mlist[t].color);
      const o = ec * 6;
      baseCol[o] = tmpColor.r * 0.5;
      baseCol[o + 1] = tmpColor.g * 0.5;
      baseCol[o + 2] = tmpColor.b * 0.5;
      baseCol[o + 3] = tmpColor.r;
      baseCol[o + 4] = tmpColor.g;
      baseCol[o + 5] = tmpColor.b;
      ec++;
    });
    edgeCount.current = ec;

    worker.current?.postMessage({
      type: "set",
      version: ++version.current,
      nodes: nodes.map((n, i) => ({ id: n.id, x: newPos[2 * i], y: newPos[2 * i + 1], r: mlist[i].r })),
      links: Array.from({ length: ec }, (_, i) => ({ source: ord[linkSrc.current[i]], target: ord[linkDst.current[i]] })),
      alpha: allKnown ? 0 : 0.9,
    });

    setNodeCount(nodes.length);
    // Always-on labels for nodes flagged `alwaysLabel`; the rest label on
    // hover/select only.
    const lbl = mlist
      .map((n, i) => ({ id: n.id, idx: i, label: n.label }))
      .filter((_, i) => mlist[i].alwaysLabel);
    setLabels(lbl);

    if (prevKey.current !== layoutKey) {
      prevKey.current = layoutKey;
      userInteracted.current = false;
    }
    if (!userInteracted.current) fitFrames.current = 140;
    dirty.current = true;
  }, [elements, layoutKey, baseCol, tmpColor]);

  // Recolor/scale when the focus changes.
  useEffect(() => {
    dirty.current = true;
  }, [hovered, selected, activeGroup]);

  // Hovered/selected nodes that aren't already always-labelled get a label too.
  const dynLabels = useMemo(() => {
    const base = labels;
    const extra: { id: string; idx: number; label: string }[] = [];
    for (const id of [hovered, selected]) {
      if (!id) continue;
      const i = idIndex.current.get(id);
      if (i === undefined) continue;
      if (!meta.current[i]?.alwaysLabel) extra.push({ id, idx: i, label: meta.current[i].label });
    }
    return extra.length ? base.concat(extra) : base;
  }, [labels, hovered, selected]);

  // ---- camera + interaction ----
  const screenToWorld = (cx: number, cy: number): THREE.Vector3 | null => {
    const { camera: cam, gl: g } = get();
    const rect = g.domElement.getBoundingClientRect();
    const ndc = new THREE.Vector2(((cx - rect.left) / rect.width) * 2 - 1, -((cy - rect.top) / rect.height) * 2 + 1);
    raycaster.setFromCamera(ndc, cam);
    const out = new THREE.Vector3();
    return raycaster.ray.intersectPlane(plane, out) ? out : null;
  };
  // Pick the node under the cursor in screen space: unproject the click to the
  // z=0 plane (the same transform node-dragging uses) and find the nearest node
  // whose disc contains it. This avoids InstancedMesh raycasting, which depends
  // on the instance matrices/material side being just right, and is robust and
  // fast (one cheap pass; only runs on pointer events).
  const pickIndex = (cx: number, cy: number): number => {
    const { gl: g } = get();
    const cam = camRef.current;
    if (!cam) return -1;
    const rect = g.domElement.getBoundingClientRect();
    if (cx < rect.left || cx > rect.right || cy < rect.top || cy > rect.bottom) return -1;
    const w = screenToWorld(cx, cy);
    if (!w) return -1;
    const p = pos.current;
    const n = order.current.length;
    // A few pixels of slack so small nodes stay clickable when zoomed out.
    const slack = 6 / cam.zoom;
    let best = -1;
    let bestD = Infinity;
    for (let i = 0; i < n; i++) {
      const dx = (p[2 * i] ?? 0) - w.x;
      const dy = (p[2 * i + 1] ?? 0) - w.y;
      const d2 = dx * dx + dy * dy;
      const hitR = (meta.current[i]?.r ?? 4) + slack;
      if (d2 <= hitR * hitR && d2 < bestD) {
        bestD = d2;
        best = i;
      }
    }
    return best;
  };
  const zoomAt = (cx: number, cy: number, factor: number) => {
    const cam = camRef.current;
    if (!cam) return;
    const before = screenToWorld(cx, cy);
    cam.zoom = clamp(cam.zoom * factor, 0.05, 80);
    cam.updateProjectionMatrix();
    const after = screenToWorld(cx, cy);
    if (before && after) {
      cam.position.x += before.x - after.x;
      cam.position.y += before.y - after.y;
      cam.updateProjectionMatrix();
    }
  };

  useEffect(() => {
    const el = gl.domElement;
    const pointers = new Map<number, { x: number; y: number }>();
    let panning = false;
    let moved = false;
    let last = { x: 0, y: 0 };
    let pinch = 0;
    let lastHover = -1;
    const take = () => {
      userInteracted.current = true;
      fitFrames.current = 0;
    };
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const cam = camRef.current;
      if (!cam) return;
      take();
      if (e.ctrlKey) zoomAt(e.clientX, e.clientY, Math.exp(-e.deltaY * 0.01));
      else {
        cam.position.x += e.deltaX / cam.zoom;
        cam.position.y -= e.deltaY / cam.zoom;
        cam.updateProjectionMatrix();
      }
    };
    const onDown = (e: PointerEvent) => {
      // Only touch pointers participate in pinch; a mouse/pen is always a single
      // pointer, so never add it to the map (a missed up/cancel must not leave a
      // phantom that makes the next click look like a two-finger pinch).
      if (e.pointerType === "touch") {
        pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
        if (pointers.size >= 2) {
          const [a, b] = [...pointers.values()];
          pinch = Math.hypot(a.x - b.x, a.y - b.y);
          panning = false;
          dragIdx.current = -1;
          return;
        }
      }
      moved = false;
      const hit = pickIndex(e.clientX, e.clientY);
      if (hit >= 0) dragIdx.current = hit;
      else {
        panning = true;
        last = { x: e.clientX, y: e.clientY };
      }
    };
    const onMove = (e: PointerEvent) => {
      if (pointers.has(e.pointerId)) pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pointers.size >= 2) {
        const [a, b] = [...pointers.values()];
        const d = Math.hypot(a.x - b.x, a.y - b.y);
        if (pinch > 0) {
          take();
          zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / pinch);
        }
        pinch = d;
        return;
      }
      if (dragIdx.current >= 0) {
        moved = true;
        take();
        const w = screenToWorld(e.clientX, e.clientY);
        if (w) {
          const i = dragIdx.current;
          pos.current[2 * i] = w.x;
          pos.current[2 * i + 1] = w.y;
          dirty.current = true;
          worker.current?.postMessage({ type: "drag", id: order.current[i], x: w.x, y: w.y });
        }
        return;
      }
      if (panning) {
        const cam = camRef.current;
        if (!cam) return;
        moved = true;
        take();
        cam.position.x -= (e.clientX - last.x) / cam.zoom;
        cam.position.y += (e.clientY - last.y) / cam.zoom;
        cam.updateProjectionMatrix();
        last = { x: e.clientX, y: e.clientY };
        return;
      }
      const h = pickIndex(e.clientX, e.clientY);
      if (h !== lastHover) {
        lastHover = h;
        setHovered(h >= 0 ? order.current[h] : null);
      }
    };
    const onUp = (e: PointerEvent) => {
      pointers.delete(e.pointerId);
      if (pointers.size < 2) pinch = 0;
      if (dragIdx.current >= 0) {
        const id = order.current[dragIdx.current];
        worker.current?.postMessage({ type: "dragEnd", id });
        if (!moved) setSelected(id);
        dragIdx.current = -1;
      } else if (panning) {
        if (!moved) setSelected(null);
        panning = false;
      }
    };
    // A cancelled gesture (force-click, focus loss, context menu, interrupted
    // touch) fires instead of pointerup; clean up so no stale state lingers.
    const onCancel = (e: PointerEvent) => {
      pointers.delete(e.pointerId);
      if (pointers.size < 2) pinch = 0;
      if (dragIdx.current >= 0) {
        worker.current?.postMessage({ type: "dragEnd", id: order.current[dragIdx.current] });
        dragIdx.current = -1;
      }
      panning = false;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("pointerdown", onDown);
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    return () => {
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("pointerdown", onDown);
      window.removeEventListener("pointercancel", onCancel);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
  }, [gl]);

  // Debug hook for e2e: WebGL nodes aren't in the DOM, so expose a way to read a
  // node's current screen position (and count) so tests can click a real node.
  useEffect(() => {
    const dbg = {
      count: () => order.current.length,
      nodeScreenPos: (i: number) => {
        const cam = camRef.current;
        const p = pos.current;
        if (!cam || i < 0 || 2 * i + 1 >= p.length) return null;
        const v = new THREE.Vector3(p[2 * i], p[2 * i + 1], 0).project(cam);
        const rect = gl.domElement.getBoundingClientRect();
        return {
          id: order.current[i],
          x: rect.left + (v.x * 0.5 + 0.5) * rect.width,
          y: rect.top + (-v.y * 0.5 + 0.5) * rect.height,
        };
      },
    };
    (window as unknown as { __graph?: typeof dbg }).__graph = dbg;
    return () => {
      delete (window as unknown as { __graph?: typeof dbg }).__graph;
    };
  }, [gl]);

  const fitView = () => {
    const cam = camRef.current;
    const n = order.current.length;
    if (!cam || !n) return;
    const p = pos.current;
    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;
    for (let i = 0; i < n; i++) {
      const x = p[2 * i],
        y = p[2 * i + 1];
      if (x < minX) minX = x;
      if (x > maxX) maxX = x;
      if (y < minY) minY = y;
      if (y > maxY) maxY = y;
    }
    if (!isFinite(minX)) return;
    const w = Math.max(maxX - minX, 1);
    const h = Math.max(maxY - minY, 1);
    const { width, height } = gl.domElement.getBoundingClientRect();
    cam.zoom = clamp(Math.min(width / (w * 1.2), height / (h * 1.2)), 0.05, 80);
    cam.position.set((minX + maxX) / 2, (minY + maxY) / 2, 100);
    cam.updateProjectionMatrix();
  };

  const labelRefs = useRef<Map<string, THREE.Object3D>>(new Map());
  const fps = useRef({ frames: 0, time: 0, worst: 0 });

  useFrame((_s, delta) => {
    if (dirty.current) updateBuffers();

    if (fitFrames.current > 0) {
      fitView();
      fitFrames.current -= 1;
    }

    const a = fps.current;
    a.frames += 1;
    a.time += delta;
    a.worst = Math.max(a.worst, delta);
    if (a.time >= 0.5) {
      if (perf) {
        perf.current.fps = Math.round(a.frames / a.time);
        perf.current.worstMs = a.worst * 1000;
      }
      a.frames = 0;
      a.time = 0;
      a.worst = 0;
    }
  });

  function updateBuffers() {
    dirty.current = false;
    const n = order.current.length;
    const p = pos.current;
    const keep = keepIdx;
    const nm = nodesMesh.current;

    // Nodes (instanced).
    if (nm) {
      nm.count = n;
      for (let i = 0; i < n; i++) {
        const m = meta.current[i];
        const sel = m.id === selected;
        const sc = m.r * (sel ? 1.6 : 1);
        dummy.position.set(p[2 * i] ?? 0, p[2 * i + 1] ?? 0, 0);
        dummy.scale.set(sc, sc, 1);
        dummy.updateMatrix();
        nm.setMatrixAt(i, dummy.matrix);
        const lit = !keep || keep.has(i);
        tmpColor.copy(m.color).multiplyScalar(lit ? 1 : 0.16);
        nm.setColorAt(i, tmpColor);
      }
      nm.instanceMatrix.needsUpdate = true;
      if (nm.instanceColor) nm.instanceColor.needsUpdate = true;
    }

    // Edges.
    const ec = edgeCount.current;
    const geom = edgeGeom.current;
    if (geom) {
      for (let i = 0; i < ec; i++) {
        const s = linkSrc.current[i];
        const t = linkDst.current[i];
        const o = i * 6;
        posArrBuf[o] = p[2 * s] ?? 0;
        posArrBuf[o + 1] = p[2 * s + 1] ?? 0;
        posArrBuf[o + 2] = 0;
        posArrBuf[o + 3] = p[2 * t] ?? 0;
        posArrBuf[o + 4] = p[2 * t + 1] ?? 0;
        posArrBuf[o + 5] = 0;
        const lit = !keep || (keep.has(s) && keep.has(t));
        const f = lit ? 1 : 0.05;
        for (let k = 0; k < 6; k++) colArrBuf[o + k] = baseCol[o + k] * (k < 3 ? f * 0.9 : f);
      }
      geom.setDrawRange(0, ec * 2);
      (geom.getAttribute("position") as THREE.BufferAttribute).needsUpdate = true;
      (geom.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
    }

    // Arrowheads (skipped on very large graphs).
    const am = arrowMesh.current;
    if (am) {
      const draw = ec <= ARROW_MAX ? ec : 0;
      am.count = draw;
      if (draw > 0) {
        for (let i = 0; i < draw; i++) {
          const s = linkSrc.current[i];
          const t = linkDst.current[i];
          const dx = (p[2 * t] ?? 0) - (p[2 * s] ?? 0);
          const dy = (p[2 * t + 1] ?? 0) - (p[2 * s + 1] ?? 0);
          const len = Math.hypot(dx, dy) || 1;
          const back = meta.current[t].r + 3.2;
          const lit = !keep || (keep.has(s) && keep.has(t));
          const sc = lit ? 1 : 0;
          dummy.position.set((p[2 * t] ?? 0) - (dx / len) * back, (p[2 * t + 1] ?? 0) - (dy / len) * back, 0);
          dummy.rotation.set(0, 0, Math.atan2(dy, dx) - Math.PI / 2);
          dummy.scale.set(sc, sc, sc);
          dummy.updateMatrix();
          am.setMatrixAt(i, dummy.matrix);
        }
        am.instanceMatrix.needsUpdate = true;
      }
    }

    // Labels follow their node.
    for (const l of dynLabels) {
      const o = labelRefs.current.get(l.id);
      if (o) o.position.set(p[2 * l.idx] ?? 0, (p[2 * l.idx + 1] ?? 0) + meta.current[l.idx].r + 2.5, 1);
    }
  }

  return (
    <>
      <OrthographicCamera ref={camRef} makeDefault position={[0, 0, 100]} zoom={4} near={0.1} far={1000} />

      <lineSegments frustumCulled={false}>
        <bufferGeometry ref={edgeGeom}>
          <bufferAttribute attach="attributes-position" args={[posArrBuf, 3]} />
          <bufferAttribute attach="attributes-color" args={[colArrBuf, 3]} />
        </bufferGeometry>
        <lineBasicMaterial vertexColors transparent />
      </lineSegments>

      <instancedMesh ref={arrowMesh} args={[undefined as any, undefined as any, MAX_EDGES]} frustumCulled={false}>
        <coneGeometry args={[2.2, 5, 3]} />
        <meshBasicMaterial color="#8a8a96" />
      </instancedMesh>

      <instancedMesh
        ref={nodesMesh}
        args={[undefined as any, undefined as any, MAX_NODES]}
        frustumCulled={false}
      >
        <circleGeometry args={[1, 16]} />
        <meshBasicMaterial toneMapped={false} />
      </instancedMesh>

      {dynLabels.map((l) => (
        <Text
          key={l.id}
          ref={(o) => {
            if (o) labelRefs.current.set(l.id, o);
            else labelRefs.current.delete(l.id);
          }}
          fontSize={meta.current[l.idx]?.fontSize ?? DEFAULT_FONT_SIZE}
          color="#e8e8ea"
          anchorX="center"
          anchorY="bottom"
          outlineWidth={0.6}
          outlineColor="#0e0e11"
        >
          {l.label}
        </Text>
      ))}
    </>
  );
}
