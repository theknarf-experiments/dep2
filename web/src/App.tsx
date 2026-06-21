import { useMemo, useRef, useState } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph } from "./ForceGraph";
import { Hud } from "./Hud";
import { useGraphData } from "./useGraphData";
import { setPaused as dbSetPaused } from "./db";
import { Mode } from "./model";

export function App() {
  const [mode, setMode] = useState<Mode>("crate");
  const [paused, setPausedState] = useState(false);
  const [hovered, setHovered] = useState<string | null>(null);
  const controls = useRef<any>(null);
  const { elements, loading } = useGraphData(mode);

  const togglePause = () => {
    const p = !paused;
    setPausedState(p);
    dbSetPaused(p);
  };

  const status = loading ? "connecting" : paused ? "paused" : "live";

  // Legend: the crate groups present, with their colors.
  const groups = useMemo(() => {
    const m = new Map<string, string>();
    for (const n of elements.nodes) if (n.kind === "crate") m.set(n.group, n.color);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, color]) => ({ name, color }));
  }, [elements.nodes]);

  return (
    <Canvas
      style={{ position: "absolute", inset: 0 }}
      gl={{ antialias: true }}
      flat
      dpr={[1, 2]}
    >
      <color attach="background" args={["#0e0e11"]} />
      <ForceGraph elements={elements} hovered={hovered} setHovered={setHovered} controls={controls} />
      <Hud
        mode={mode}
        setMode={setMode}
        paused={paused}
        togglePause={togglePause}
        status={status}
        counts={{ nodes: elements.nodes.length, edges: elements.edges.length }}
        groups={groups}
        controls={controls}
      />
    </Canvas>
  );
}
