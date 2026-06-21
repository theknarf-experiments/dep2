/// <reference lib="webworker" />
// Force-directed layout runs here, off the main thread. The main thread sends
// the graph ("set") and drag updates; we run d3-force and post back node
// positions (a Float32Array in the order the nodes were sent) every tick.

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

interface N extends SimulationNodeDatum {
  id: string;
  r: number;
}
type L = SimulationLinkDatum<N>;

interface SetMsg {
  type: "set";
  version: number;
  nodes: { id: string; x: number; y: number; r: number }[];
  links: { source: string; target: string }[];
  alpha: number;
}
interface DragMsg {
  type: "drag";
  id: string;
  x: number;
  y: number;
}
interface DragEndMsg {
  type: "dragEnd";
  id: string;
}
type InMsg = SetMsg | DragMsg | DragEndMsg;

let sim: Simulation<N, L> | null = null;
let nodes: N[] = [];
let version = 0;
const byId = new Map<string, N>();

function tick() {
  const pos = new Float32Array(nodes.length * 2);
  for (let i = 0; i < nodes.length; i++) {
    pos[2 * i] = nodes[i].x ?? 0;
    pos[2 * i + 1] = nodes[i].y ?? 0;
  }
  (self as DedicatedWorkerGlobalScope).postMessage({ type: "tick", version, pos }, [pos.buffer]);
}

function set(msg: SetMsg) {
  version = msg.version;
  byId.clear();
  nodes = msg.nodes.map((n) => {
    const o: N = { id: n.id, r: n.r, x: n.x, y: n.y };
    byId.set(n.id, o);
    return o;
  });
  const links: L[] = msg.links
    .filter((l) => byId.has(l.source) && byId.has(l.target))
    .map((l) => ({ source: l.source, target: l.target }));

  if (!sim) {
    sim = forceSimulation<N, L>(nodes)
      .force("charge", forceManyBody<N>().strength(-240))
      .force(
        "link",
        forceLink<N, L>(links)
          .id((d) => d.id)
          .distance(38)
          .strength(0.45),
      )
      .force("x", forceX<N>(0).strength(0.045))
      .force("y", forceY<N>(0).strength(0.045))
      .force("collide", forceCollide<N>((d) => d.r + 4));
    sim.on("tick", tick);
  } else {
    sim.nodes(nodes);
    (sim.force("link") as ReturnType<typeof forceLink<N, L>>).links(links);
  }
  sim.alpha(msg.alpha).restart();
}

self.onmessage = (e: MessageEvent<InMsg>) => {
  const m = e.data;
  if (m.type === "set") {
    set(m);
  } else if (m.type === "drag") {
    const n = byId.get(m.id);
    if (n) {
      n.fx = m.x;
      n.fy = m.y;
    }
    sim?.alphaTarget(0.3).restart();
  } else if (m.type === "dragEnd") {
    const n = byId.get(m.id);
    if (n) {
      n.fx = null;
      n.fy = null;
    }
    sim?.alphaTarget(0);
  }
};
