import { useEffect, useMemo, useRef, useState } from "react";
import { useFrame, useThree } from "@react-three/fiber";
import { OrthographicCamera, OrbitControls, Text } from "@react-three/drei";
import * as THREE from "three";
import {
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  Simulation,
  SimulationLinkDatum,
  SimulationNodeDatum,
} from "d3-force";
import { GraphElements } from "./model";

interface SimNode extends SimulationNodeDatum {
  id: string;
  label: string;
  group: string;
  kind: "crate" | "file";
  color: string;
  r: number;
}
type SimLink = SimulationLinkDatum<SimNode> & { id: string };

const radiusFor = (kind: "crate" | "file") => (kind === "crate" ? 9 : 4);
// Fixed instance/vertex capacity; we draw only the first `count` each frame, so
// the buffers are never reallocated and no leftover instance can sit at origin.
const MAX_EDGES = 8192;

interface Props {
  elements: GraphElements;
  hovered: string | null;
  setHovered: (id: string | null) => void;
}

export function ForceGraph({ elements, hovered, setHovered }: Props) {
  const { camera, size } = useThree();
  const get = useThree((s) => s.get);
  const controls = useRef<any>(null);

  const sim = useRef<Simulation<SimNode, SimLink> | null>(null);
  const nodesMap = useRef<Map<string, SimNode>>(new Map());
  const nodesArr = useRef<SimNode[]>([]);
  const linksArr = useRef<SimLink[]>([]);
  const groupRefs = useRef<Map<string, THREE.Group>>(new Map());
  const edgeGeom = useRef<THREE.BufferGeometry>(null);
  const arrowMesh = useRef<THREE.InstancedMesh>(null);
  const needsFit = useRef(true);
  const dragId = useRef<string | null>(null);

  const [nodeList, setNodeList] = useState<SimNode[]>([]);

  // Fixed-capacity edge buffers. `baseCol` holds the un-dimmed gradient; the live
  // color attribute is baseCol * hover-factor, recomputed each frame.
  const posArr = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const colArr = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const baseCol = useMemo(() => new Float32Array(MAX_EDGES * 6), []);
  const dummy = useMemo(() => new THREE.Object3D(), []);

  const neighbors = useMemo(() => {
    if (!hovered) return null;
    const set = new Set<string>([hovered]);
    for (const e of elements.edges) {
      if (e.source === hovered) set.add(e.target);
      if (e.target === hovered) set.add(e.source);
    }
    return set;
  }, [hovered, elements.edges]);

  useEffect(() => {
    sim.current = forceSimulation<SimNode, SimLink>()
      .force("charge", forceManyBody<SimNode>().strength(-240))
      .force(
        "link",
        forceLink<SimNode, SimLink>()
          .id((d) => d.id)
          .distance(38)
          .strength(0.45),
      )
      .force("x", forceX<SimNode>(0).strength(0.045))
      .force("y", forceY<SimNode>(0).strength(0.045))
      .force("collide", forceCollide<SimNode>((d) => d.r + 4))
      .stop();
    return () => {
      sim.current?.stop();
    };
  }, []);

  // Reconcile elements -> sim nodes/links, preserving positions of persisting
  // nodes so live updates don't reshuffle the whole graph.
  useEffect(() => {
    const map = nodesMap.current;
    const seen = new Set<string>();
    for (const n of elements.nodes) {
      seen.add(n.id);
      const r = radiusFor(n.kind);
      const existing = map.get(n.id);
      if (existing) {
        Object.assign(existing, { label: n.label, group: n.group, color: n.color, kind: n.kind, r });
      } else {
        map.set(n.id, {
          id: n.id,
          label: n.label,
          group: n.group,
          color: n.color,
          kind: n.kind,
          r,
          x: (Math.random() - 0.5) * 60,
          y: (Math.random() - 0.5) * 60,
        });
      }
    }
    for (const id of [...map.keys()]) if (!seen.has(id)) map.delete(id);

    nodesArr.current = [...map.values()];
    linksArr.current = elements.edges
      .filter((e) => map.has(e.source) && map.has(e.target))
      .slice(0, MAX_EDGES)
      .map((e) => ({ id: e.id, source: e.source, target: e.target }));

    // Bake the direction gradient (dim source -> bright target) into baseCol now,
    // while source/target are still id strings — forceLink.links() below rewrites
    // them in place to node-object references.
    const c = new THREE.Color();
    linksArr.current.forEach((l, i) => {
      const tgt = nodesMap.current.get(l.target as string);
      c.set(tgt?.color ?? "#888888");
      const o = i * 6;
      baseCol[o] = c.r * 0.5;
      baseCol[o + 1] = c.g * 0.5;
      baseCol[o + 2] = c.b * 0.5;
      baseCol[o + 3] = c.r;
      baseCol[o + 4] = c.g;
      baseCol[o + 5] = c.b;
    });

    const s = sim.current;
    if (s) {
      s.nodes(nodesArr.current);
      (s.force("link") as ReturnType<typeof forceLink<SimNode, SimLink>>).links(linksArr.current);
      s.alpha(0.9);
    }

    setNodeList(nodesArr.current.slice());
    needsFit.current = true;
  }, [elements, baseCol]);

  const raycaster = useMemo(() => new THREE.Raycaster(), []);
  const plane = useMemo(() => new THREE.Plane(new THREE.Vector3(0, 0, 1), 0), []);
  // Read camera/gl live via get(); the drag listeners are registered once, so a
  // captured camera would be stale and unproject everything to the origin.
  const screenToWorld = (cx: number, cy: number): THREE.Vector3 | null => {
    const { camera: cam, gl } = get();
    const rect = gl.domElement.getBoundingClientRect();
    const ndc = new THREE.Vector2(
      ((cx - rect.left) / rect.width) * 2 - 1,
      -((cy - rect.top) / rect.height) * 2 + 1,
    );
    raycaster.setFromCamera(ndc, cam);
    const out = new THREE.Vector3();
    return raycaster.ray.intersectPlane(plane, out) ? out : null;
  };

  useEffect(() => {
    const move = (ev: PointerEvent) => {
      if (!dragId.current) return;
      const n = nodesMap.current.get(dragId.current);
      if (!n) return;
      const p = screenToWorld(ev.clientX, ev.clientY);
      if (!p) return;
      n.fx = p.x;
      n.fy = p.y;
    };
    const up = () => {
      if (!dragId.current) return;
      const n = nodesMap.current.get(dragId.current);
      if (n) {
        n.fx = null;
        n.fy = null;
      }
      dragId.current = null;
      sim.current?.alphaTarget(0);
      if (controls.current) controls.current.enabled = true;
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
  }, []);

  const startDrag = (e: any, n: SimNode) => {
    e.stopPropagation();
    dragId.current = n.id;
    n.fx = n.x;
    n.fy = n.y;
    sim.current?.alphaTarget(0.3).alpha(0.5);
    if (controls.current) controls.current.enabled = false;
  };

  const fitView = () => {
    const ns = nodesArr.current;
    if (!ns.length) return;
    let minX = Infinity,
      minY = Infinity,
      maxX = -Infinity,
      maxY = -Infinity;
    for (const n of ns) {
      minX = Math.min(minX, n.x ?? 0);
      maxX = Math.max(maxX, n.x ?? 0);
      minY = Math.min(minY, n.y ?? 0);
      maxY = Math.max(maxY, n.y ?? 0);
    }
    const w = Math.max(maxX - minX, 1);
    const h = Math.max(maxY - minY, 1);
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    const cam = camera as THREE.OrthographicCamera;
    cam.zoom = Math.min(size.width / (w * 1.25), size.height / (h * 1.25));
    cam.position.set(cx, cy, 100);
    cam.updateProjectionMatrix();
    if (controls.current) {
      controls.current.target.set(cx, cy, 0);
      controls.current.update();
    }
  };

  useFrame(() => {
    const s = sim.current;
    if (!s) return;
    if (s.alpha() > s.alphaMin()) s.tick();

    const dim = neighbors;
    const count = linksArr.current.length;
    // After forceLink runs, l.source/l.target are node objects; before the first
    // tick they may still be id strings. Resolve either form.
    const nodeOf = (e: string | SimNode): SimNode | undefined =>
      typeof e === "object" ? e : nodesMap.current.get(e);

    // Nodes.
    for (const n of nodesArr.current) {
      const g = groupRefs.current.get(n.id);
      if (!g) continue;
      g.position.set(n.x ?? 0, n.y ?? 0, 0);
      const mesh = g.children[0] as THREE.Mesh | undefined;
      if (mesh) {
        const m = mesh.material as THREE.MeshBasicMaterial;
        m.transparent = true;
        m.opacity = !dim || dim.has(n.id) ? 1 : 0.12;
      }
    }

    // Edge segments + hover dimming.
    const geom = edgeGeom.current;
    if (geom) {
      for (let i = 0; i < count; i++) {
        const l = linksArr.current[i];
        const sN = nodeOf(l.source as string | SimNode);
        const tN = nodeOf(l.target as string | SimNode);
        const o = i * 6;
        if (sN && tN) {
          posArr[o] = sN.x ?? 0;
          posArr[o + 1] = sN.y ?? 0;
          posArr[o + 2] = 0;
          posArr[o + 3] = tN.x ?? 0;
          posArr[o + 4] = tN.y ?? 0;
          posArr[o + 5] = 0;
        }
        const lit = !dim || (!!sN && !!tN && dim.has(sN.id) && dim.has(tN.id));
        const f = lit ? 1 : 0.06;
        for (let k = 0; k < 6; k++) colArr[o + k] = baseCol[o + k] * f;
      }
      geom.setDrawRange(0, count * 2);
      (geom.getAttribute("position") as THREE.BufferAttribute).needsUpdate = true;
      (geom.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
    }

    // Arrowheads: draw exactly `count`; never leave a stray instance at origin.
    const am = arrowMesh.current;
    if (am) {
      am.count = count;
      for (let i = 0; i < count; i++) {
        const l = linksArr.current[i];
        const sN = nodeOf(l.source as string | SimNode);
        const tN = nodeOf(l.target as string | SimNode);
        if (!sN || !tN) {
          dummy.scale.set(0, 0, 0);
          dummy.updateMatrix();
          am.setMatrixAt(i, dummy.matrix);
          continue;
        }
        const dx = (tN.x ?? 0) - (sN.x ?? 0);
        const dy = (tN.y ?? 0) - (sN.y ?? 0);
        const len = Math.hypot(dx, dy) || 1;
        const back = tN.r + 3.2;
        const lit = !dim || (dim.has(sN.id) && dim.has(tN.id));
        const sc = lit ? 1 : 0;
        dummy.position.set((tN.x ?? 0) - (dx / len) * back, (tN.y ?? 0) - (dy / len) * back, 0);
        dummy.rotation.set(0, 0, Math.atan2(dy, dx) - Math.PI / 2);
        dummy.scale.set(sc, sc, sc);
        dummy.updateMatrix();
        am.setMatrixAt(i, dummy.matrix);
      }
      am.instanceMatrix.needsUpdate = true;
    }

    // Keep the graph framed while it expands; lock once it has settled so we
    // don't fight the user's pan/zoom afterwards.
    if (needsFit.current) {
      fitView();
      if (s.alpha() < 0.04) needsFit.current = false;
    }
  });

  return (
    <>
      <OrthographicCamera makeDefault position={[0, 0, 100]} zoom={4} near={0.1} far={1000} />
      <OrbitControls
        ref={controls}
        makeDefault
        enableRotate={false}
        screenSpacePanning
        mouseButtons={{
          LEFT: THREE.MOUSE.PAN,
          MIDDLE: THREE.MOUSE.DOLLY,
          RIGHT: THREE.MOUSE.PAN,
        }}
      />

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
            else groupRefs.current.delete(n.id);
          }}
        >
          <mesh
            onPointerOver={(e) => {
              e.stopPropagation();
              setHovered(n.id);
            }}
            onPointerOut={() => setHovered(null)}
            onPointerDown={(e) => startDrag(e, n)}
          >
            <circleGeometry args={[n.r, n.kind === "crate" ? 24 : 14]} />
            <meshBasicMaterial color={n.color} />
          </mesh>
          {(n.kind === "crate" || n.id === hovered) && (
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
