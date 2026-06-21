import { MutableRefObject, useEffect, useMemo, useRef, useState } from "react";
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

interface Props {
  elements: GraphElements;
  hovered: string | null;
  setHovered: (id: string | null) => void;
  controls: MutableRefObject<any>;
}

export function ForceGraph({ elements, hovered, setHovered, controls }: Props) {
  const { camera, size } = useThree();
  const get = useThree((s) => s.get);

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
  const [edge, setEdge] = useState<{ pos: Float32Array; col: Float32Array; count: number }>({
    pos: new Float32Array(0),
    col: new Float32Array(0),
    count: 0,
  });

  // Neighborhood of the hovered node (for dimming everything else).
  const neighbors = useMemo(() => {
    if (!hovered) return null;
    const set = new Set<string>([hovered]);
    for (const e of elements.edges) {
      if (e.source === hovered) set.add(e.target);
      if (e.target === hovered) set.add(e.source);
    }
    return set;
  }, [hovered, elements.edges]);

  // Create the simulation once. We tick it manually in useFrame (no internal
  // timer), so it stays in lockstep with the render loop.
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

  // Reconcile elements -> sim nodes/links, preserving positions of nodes that
  // persist so live updates don't reshuffle the whole graph.
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
      .map((e) => ({ id: e.id, source: e.source, target: e.target }));

    const s = sim.current;
    if (s) {
      s.nodes(nodesArr.current);
      (s.force("link") as ReturnType<typeof forceLink<SimNode, SimLink>>).links(linksArr.current);
      s.alpha(0.9); // re-energize; manual ticks in useFrame consume it
    }

    // (Re)allocate edge buffers; colors fade source -> target to show direction.
    const count = linksArr.current.length;
    const pos = new Float32Array(count * 6);
    const col = new Float32Array(count * 6);
    const c = new THREE.Color();
    linksArr.current.forEach((l, i) => {
      const tgt = nodesMap.current.get(l.target as string);
      c.set(tgt?.color ?? "#888888");
      const o = i * 6;
      // source vertex (dim)
      col[o] = c.r * 0.35;
      col[o + 1] = c.g * 0.35;
      col[o + 2] = c.b * 0.35;
      // target vertex (bright)
      col[o + 3] = c.r;
      col[o + 4] = c.g;
      col[o + 5] = c.b;
    });
    setEdge({ pos, col, count });
    setNodeList(nodesArr.current.slice());
    needsFit.current = true;
  }, [elements]);

  // Screen px -> world point on the z=0 plane (for dragging).
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

  // Drag: pin the node under the pointer; disable camera controls meanwhile.
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

  const dummy = useMemo(() => new THREE.Object3D(), []);

  useFrame(() => {
    const s = sim.current;
    if (!s) return;
    if (s.alpha() > s.alphaMin()) s.tick();

    // Node positions.
    const dim = neighbors;
    for (const n of nodesArr.current) {
      const g = groupRefs.current.get(n.id);
      if (!g) continue;
      g.position.set(n.x ?? 0, n.y ?? 0, 0);
      const on = !dim || dim.has(n.id);
      g.visible = true;
      const mesh = g.children[0] as THREE.Mesh | undefined;
      if (mesh) {
        const m = mesh.material as THREE.MeshBasicMaterial;
        m.opacity = on ? 1 : 0.12;
        m.transparent = true;
      }
    }

    // Edge segment endpoints + per-edge dim when hovering.
    const geom = edgeGeom.current;
    if (geom && edge.count) {
      const pa = edge.pos;
      const ca = (geom.getAttribute("color") as THREE.BufferAttribute).array as Float32Array;
      linksArr.current.forEach((l, i) => {
        const sN = nodesMap.current.get(l.source as string);
        const tN = nodesMap.current.get(l.target as string);
        const o = i * 6;
        if (sN && tN) {
          pa[o] = sN.x ?? 0;
          pa[o + 1] = sN.y ?? 0;
          pa[o + 2] = 0;
          pa[o + 3] = tN.x ?? 0;
          pa[o + 4] = tN.y ?? 0;
          pa[o + 5] = 0;
        }
        const lit = !dim || (l.source && l.target && dim.has(l.source as string) && dim.has(l.target as string));
        const f = lit ? 1 : 0.08;
        for (let k = 0; k < 6; k++) ca[o + k] = edge.col[o + k] * (k < 3 ? f * 0.9 : f);
      });
      (geom.getAttribute("position") as THREE.BufferAttribute).needsUpdate = true;
      (geom.getAttribute("color") as THREE.BufferAttribute).needsUpdate = true;
    }

    // Arrowheads near the target end of each edge.
    const am = arrowMesh.current;
    if (am && edge.count) {
      linksArr.current.forEach((l, i) => {
        const sN = nodesMap.current.get(l.source as string);
        const tN = nodesMap.current.get(l.target as string);
        if (!sN || !tN) return;
        const dx = (tN.x ?? 0) - (sN.x ?? 0);
        const dy = (tN.y ?? 0) - (sN.y ?? 0);
        const len = Math.hypot(dx, dy) || 1;
        const ux = dx / len;
        const uy = dy / len;
        const back = tN.r + 3.2;
        dummy.position.set((tN.x ?? 0) - ux * back, (tN.y ?? 0) - uy * back, 0);
        dummy.rotation.set(0, 0, Math.atan2(dy, dx) - Math.PI / 2);
        const lit = !dim || (dim.has(l.source as string) && dim.has(l.target as string));
        const sc = lit ? 1 : 0.0001;
        dummy.scale.set(sc, sc, sc);
        dummy.updateMatrix();
        am.setMatrixAt(i, dummy.matrix);
      });
      am.instanceMatrix.needsUpdate = true;
    }

    if (needsFit.current && s.alpha() < 0.2) {
      fitView();
      needsFit.current = false;
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

      {/* edges */}
      <lineSegments key={`edges-${edge.count}`} frustumCulled={false}>
        <bufferGeometry ref={edgeGeom}>
          <bufferAttribute attach="attributes-position" args={[edge.pos, 3]} count={edge.count * 2} />
          <bufferAttribute attach="attributes-color" args={[edge.col, 3]} count={edge.count * 2} />
        </bufferGeometry>
        <lineBasicMaterial vertexColors transparent />
      </lineSegments>

      {/* arrowheads (only once there are edges, else a lone cone sits at origin) */}
      {edge.count > 0 && (
        <instancedMesh
          key={edge.count}
          ref={arrowMesh}
          args={[undefined as any, undefined as any, edge.count]}
          frustumCulled={false}
        >
          <coneGeometry args={[2.2, 5, 3]} />
          <meshBasicMaterial color="#8a8a96" />
        </instancedMesh>
      )}

      {/* nodes */}
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
