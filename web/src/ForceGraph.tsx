import { MutableRefObject, useEffect, useMemo, useRef, useState } from "react";
import { useFrame, useThree } from "@react-three/fiber";
import { OrthographicCamera, Text } from "@react-three/drei";
import * as THREE from "three";
import { GraphElements } from "./model";
import { Perf } from "./perf";

interface SimNode {
  id: string;
  label: string;
  group: string;
  kind: "crate" | "file";
  color: string;
  r: number;
}

const radiusFor = (kind: "crate" | "file") => (kind === "crate" ? 9 : 4);
const MAX_EDGES = 8192;
const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

interface Props {
  elements: GraphElements;
  hovered: string | null;
  setHovered: (id: string | null) => void;
  selected: string | null;
  setSelected: (id: string | null) => void;
  perf: MutableRefObject<Perf>;
}

export function ForceGraph({ elements, hovered, setHovered, selected, setSelected, perf }: Props) {
  const { gl } = useThree();
  const camRef = useRef<THREE.OrthographicCamera>(null);

  // Layout worker + the latest positions it has produced.
  const worker = useRef<Worker | null>(null);
  const order = useRef<string[]>([]); // node ids, in the order sent to the worker
  const version = useRef(0);
  const posRef = useRef<Map<string, [number, number]>>(new Map());

  const nodesArr = useRef<SimNode[]>([]);
  const linksArr = useRef<{ source: string; target: string }[]>([]);
  const groupRefs = useRef<Map<string, THREE.Group>>(new Map());
  const meshRefs = useRef<Map<string, THREE.Mesh>>(new Map());
  const edgeGeom = useRef<THREE.BufferGeometry>(null);
  const arrowMesh = useRef<THREE.InstancedMesh>(null);
  const fitFrames = useRef(0);
  const dragId = useRef<string | null>(null);

  const [nodeList, setNodeList] = useState<SimNode[]>([]);

  const posArr = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const colArr = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const baseCol = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const dummy = useMemo(() => new THREE.Object3D(), []);
  const raycaster = useMemo(() => new THREE.Raycaster(), []);
  const plane = useMemo(() => new THREE.Plane(new THREE.Vector3(0, 0, 1), 0), []);

  // Dim everything outside the focused node's neighborhood. Focus follows hover,
  // falling back to the current selection so a clicked node stays highlighted.
  const focus = hovered ?? selected;
  const neighbors = useMemo(() => {
    if (!focus) return null;
    const set = new Set<string>([focus]);
    for (const e of elements.edges) {
      if (e.source === focus) set.add(e.target);
      if (e.target === focus) set.add(e.source);
    }
    return set;
  }, [focus, elements.edges]);

  // Spin up the layout worker once.
  useEffect(() => {
    const w = new Worker(new URL("./forceWorker.ts", import.meta.url), { type: "module" });
    worker.current = w;
    w.onmessage = (e: MessageEvent) => {
      const m = e.data;
      if (m.type === "tick" && m.version === version.current) {
        const pos = m.pos as Float32Array;
        const ord = order.current;
        for (let i = 0; i < ord.length; i++) posRef.current.set(ord[i], [pos[2 * i], pos[2 * i + 1]]);
      }
    };
    return () => w.terminate();
  }, []);

  // Reconcile elements -> worker graph, preserving known positions.
  useEffect(() => {
    const colorById = new Map(elements.nodes.map((n) => [n.id, n.color]));
    nodesArr.current = elements.nodes.map((n) => ({
      id: n.id,
      label: n.label,
      group: n.group,
      kind: n.kind,
      color: n.color,
      r: radiusFor(n.kind),
    }));
    const present = new Set(elements.nodes.map((n) => n.id));
    linksArr.current = elements.edges
      .filter((e) => present.has(e.source) && present.has(e.target))
      .slice(0, MAX_EDGES);

    // Bake the source->target color gradient for the edges.
    const c = new THREE.Color();
    linksArr.current.forEach((l, i) => {
      c.set(colorById.get(l.target) ?? "#888888");
      const o = i * 6;
      baseCol[o] = c.r * 0.5;
      baseCol[o + 1] = c.g * 0.5;
      baseCol[o + 2] = c.b * 0.5;
      baseCol[o + 3] = c.r;
      baseCol[o + 4] = c.g;
      baseCol[o + 5] = c.b;
    });

    // Seed positions (reuse known, random for new) and hand the graph to the worker.
    version.current += 1;
    order.current = nodesArr.current.map((n) => n.id);
    const wnodes = nodesArr.current.map((n) => {
      const p = posRef.current.get(n.id) ?? [(Math.random() - 0.5) * 60, (Math.random() - 0.5) * 60];
      posRef.current.set(n.id, p);
      return { id: n.id, x: p[0], y: p[1], r: n.r };
    });
    // Drop positions for nodes that no longer exist.
    for (const id of [...posRef.current.keys()]) if (!present.has(id)) posRef.current.delete(id);

    worker.current?.postMessage({
      type: "set",
      version: version.current,
      nodes: wnodes,
      links: linksArr.current,
      alpha: 0.9,
    });

    setNodeList(nodesArr.current.slice());
    fitFrames.current = 140;
  }, [elements, baseCol]);

  // ---- camera helpers ----
  const screenToWorld = (cx: number, cy: number): THREE.Vector3 | null => {
    const cam = camRef.current;
    if (!cam) return null;
    const rect = gl.domElement.getBoundingClientRect();
    const ndc = new THREE.Vector2(
      ((cx - rect.left) / rect.width) * 2 - 1,
      -((cy - rect.top) / rect.height) * 2 + 1,
    );
    raycaster.setFromCamera(ndc, cam);
    const out = new THREE.Vector3();
    return raycaster.ray.intersectPlane(plane, out) ? out : null;
  };

  const pick = (cx: number, cy: number): string | null => {
    const cam = camRef.current;
    if (!cam) return null;
    const rect = gl.domElement.getBoundingClientRect();
    if (cx < rect.left || cx > rect.right || cy < rect.top || cy > rect.bottom) return null;
    const ndc = new THREE.Vector2(
      ((cx - rect.left) / rect.width) * 2 - 1,
      -((cy - rect.top) / rect.height) * 2 + 1,
    );
    raycaster.setFromCamera(ndc, cam);
    const hits = raycaster.intersectObjects([...meshRefs.current.values()], false);
    return hits.length ? (hits[0].object.userData.id as string) : null;
  };

  const zoomAt = (cx: number, cy: number, factor: number) => {
    const cam = camRef.current;
    if (!cam) return;
    const before = screenToWorld(cx, cy);
    cam.zoom = clamp(cam.zoom * factor, 0.2, 60);
    cam.updateProjectionMatrix();
    const after = screenToWorld(cx, cy);
    if (before && after) {
      cam.position.x += before.x - after.x;
      cam.position.y += before.y - after.y;
      cam.updateProjectionMatrix();
    }
  };

  // ---- interaction: wheel (pan / ctrl+pinch zoom), drag (pan / node), hover, click ----
  useEffect(() => {
    const el = gl.domElement;
    const pointers = new Map<number, { x: number; y: number }>();
    let panning = false;
    let pannedOrDragged = false;
    let last = { x: 0, y: 0 };
    let pinchDist = 0;
    let lastHover: string | null = null;

    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const cam = camRef.current;
      if (!cam) return;
      if (e.ctrlKey) {
        // ctrl+scroll and trackpad pinch (macOS reports pinch as ctrl+wheel).
        zoomAt(e.clientX, e.clientY, Math.exp(-e.deltaY * 0.01));
      } else {
        // two-finger scroll pans.
        cam.position.x += e.deltaX / cam.zoom;
        cam.position.y -= e.deltaY / cam.zoom;
        cam.updateProjectionMatrix();
      }
    };

    const onDown = (e: PointerEvent) => {
      pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        pinchDist = Math.hypot(a.x - b.x, a.y - b.y);
        panning = false;
        dragId.current = null;
        return;
      }
      pannedOrDragged = false;
      const hit = pick(e.clientX, e.clientY);
      if (hit) {
        dragId.current = hit;
      } else {
        panning = true;
        last = { x: e.clientX, y: e.clientY };
      }
    };

    const onMove = (e: PointerEvent) => {
      if (pointers.has(e.pointerId)) pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });

      if (pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        const d = Math.hypot(a.x - b.x, a.y - b.y);
        if (pinchDist > 0) zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / pinchDist);
        pinchDist = d;
        return;
      }

      if (dragId.current) {
        pannedOrDragged = true;
        const p = screenToWorld(e.clientX, e.clientY);
        if (p) {
          posRef.current.set(dragId.current, [p.x, p.y]);
          worker.current?.postMessage({ type: "drag", id: dragId.current, x: p.x, y: p.y });
        }
        return;
      }
      if (panning) {
        const cam = camRef.current;
        if (!cam) return;
        pannedOrDragged = true;
        cam.position.x -= (e.clientX - last.x) / cam.zoom;
        cam.position.y += (e.clientY - last.y) / cam.zoom;
        cam.updateProjectionMatrix();
        last = { x: e.clientX, y: e.clientY };
        return;
      }
      // hover (only push state when the hovered node actually changes)
      const h = pick(e.clientX, e.clientY);
      if (h !== lastHover) {
        lastHover = h;
        setHovered(h);
      }
    };

    const onUp = (e: PointerEvent) => {
      pointers.delete(e.pointerId);
      if (pointers.size < 2) pinchDist = 0;
      if (dragId.current) {
        const id = dragId.current;
        worker.current?.postMessage({ type: "dragEnd", id });
        if (!pannedOrDragged) setSelected(id); // a click, not a drag
        dragId.current = null;
      } else if (panning) {
        if (!pannedOrDragged) setSelected(null); // background click clears selection
        panning = false;
      }
    };

    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("pointerdown", onDown);
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    return () => {
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("pointerdown", onDown);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
  }, [gl]);

  const fitView = () => {
    const cam = camRef.current;
    if (!cam || !nodesArr.current.length) return;
    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;
    for (const n of nodesArr.current) {
      const p = posRef.current.get(n.id);
      if (!p) continue;
      minX = Math.min(minX, p[0]);
      maxX = Math.max(maxX, p[0]);
      minY = Math.min(minY, p[1]);
      maxY = Math.max(maxY, p[1]);
    }
    if (!isFinite(minX)) return;
    const w = Math.max(maxX - minX, 1);
    const h = Math.max(maxY - minY, 1);
    const { width, height } = gl.domElement.getBoundingClientRect();
    cam.zoom = clamp(Math.min(width / (w * 1.25), height / (h * 1.25)), 0.2, 60);
    cam.position.set((minX + maxX) / 2, (minY + maxY) / 2, 100);
    cam.updateProjectionMatrix();
  };

  // ---- per-frame render + fps ----
  const fpsAccum = useRef({ frames: 0, time: 0, worst: 0 });
  useFrame((_state, delta) => {
    const dim = neighbors;
    const count = linksArr.current.length;
    const pos = posRef.current;

    for (const n of nodesArr.current) {
      const g = groupRefs.current.get(n.id);
      const p = pos.get(n.id);
      if (!g || !p) continue;
      g.position.set(p[0], p[1], 0);
      const mesh = g.children[0] as THREE.Mesh | undefined;
      if (mesh) {
        const m = mesh.material as THREE.MeshBasicMaterial;
        m.transparent = true;
        m.opacity = !dim || dim.has(n.id) ? 1 : 0.12;
        const sel = n.id === selected;
        mesh.scale.setScalar(sel ? 1.5 : 1);
      }
    }

    const geom = edgeGeom.current;
    if (geom) {
      for (let i = 0; i < count; i++) {
        const l = linksArr.current[i];
        const s = pos.get(l.source);
        const t = pos.get(l.target);
        const o = i * 6;
        if (s && t) {
          posArr[o] = s[0];
          posArr[o + 1] = s[1];
          posArr[o + 2] = 0;
          posArr[o + 3] = t[0];
          posArr[o + 4] = t[1];
          posArr[o + 5] = 0;
        }
        const lit = !dim || (dim.has(l.source) && dim.has(l.target));
        const f = lit ? 1 : 0.06;
        for (let k = 0; k < 6; k++) colArr[o + k] = baseCol[o + k] * f;
      }
      geom.setDrawRange(0, count * 2);
      (geom.getAttribute("position") as THREE.BufferAttribute).needsUpdate = true;
      (geom.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
    }

    const am = arrowMesh.current;
    if (am) {
      am.count = count;
      for (let i = 0; i < count; i++) {
        const l = linksArr.current[i];
        const s = pos.get(l.source);
        const t = pos.get(l.target);
        const tn = nodesArr.current.find((n) => n.id === l.target);
        if (!s || !t || !tn) {
          dummy.scale.set(0, 0, 0);
          dummy.updateMatrix();
          am.setMatrixAt(i, dummy.matrix);
          continue;
        }
        const dx = t[0] - s[0];
        const dy = t[1] - s[1];
        const len = Math.hypot(dx, dy) || 1;
        const back = tn.r + 3.2;
        const lit = !dim || (dim.has(l.source) && dim.has(l.target));
        const sc = lit ? 1 : 0;
        dummy.position.set(t[0] - (dx / len) * back, t[1] - (dy / len) * back, 0);
        dummy.rotation.set(0, 0, Math.atan2(dy, dx) - Math.PI / 2);
        dummy.scale.set(sc, sc, sc);
        dummy.updateMatrix();
        am.setMatrixAt(i, dummy.matrix);
      }
      am.instanceMatrix.needsUpdate = true;
    }

    if (fitFrames.current > 0) {
      fitView();
      fitFrames.current -= 1;
    }

    // fps / worst-frame over ~500ms windows
    const a = fpsAccum.current;
    a.frames += 1;
    a.time += delta;
    a.worst = Math.max(a.worst, delta);
    if (a.time >= 0.5) {
      perf.current.fps = Math.round(a.frames / a.time);
      perf.current.worstMs = a.worst * 1000;
      a.frames = 0;
      a.time = 0;
      a.worst = 0;
    }
  });

  return (
    <>
      <OrthographicCamera ref={camRef} makeDefault position={[0, 0, 100]} zoom={4} near={0.1} far={1000} />

      <lineSegments frustumCulled={false}>
        <bufferGeometry ref={edgeGeom}>
          <bufferAttribute attach="attributes-position" args={[posArr, 3]} />
          <bufferAttribute attach="attributes-color" args={[colArr, 3]} />
        </bufferGeometry>
        <lineBasicMaterial vertexColors transparent />
      </lineSegments>

      <instancedMesh
        ref={arrowMesh}
        args={[undefined as any, undefined as any, MAX_EDGES]}
        frustumCulled={false}
      >
        <coneGeometry args={[2.2, 5, 3]} />
        <meshBasicMaterial color="#8a8a96" />
      </instancedMesh>

      {nodeList.map((n) => (
        <group
          key={n.id}
          ref={(o) => {
            if (o) groupRefs.current.set(n.id, o);
            else {
              groupRefs.current.delete(n.id);
              meshRefs.current.delete(n.id);
            }
          }}
        >
          <mesh
            ref={(m) => {
              if (m) {
                m.userData.id = n.id;
                meshRefs.current.set(n.id, m);
              }
            }}
          >
            <circleGeometry args={[n.r, n.kind === "crate" ? 24 : 14]} />
            <meshBasicMaterial color={n.color} />
          </mesh>
          {(n.kind === "crate" || n.id === hovered || n.id === selected) && (
            <Text
              position={[0, n.r + 2.5, 1]}
              fontSize={n.kind === "crate" ? 6 : 4.5}
              color="#e8e8ea"
              anchorX="center"
              anchorY="bottom"
              outlineWidth={0.6}
              outlineColor="#0e0e11"
            >
              {n.label}
            </Text>
          )}
        </group>
      ))}
    </>
  );
}
