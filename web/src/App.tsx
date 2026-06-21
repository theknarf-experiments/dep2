import { useMemo, useRef, useState } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph } from "./ForceGraph";
import { Hud } from "./Hud";
import { useGraphData } from "./useGraphData";
import { setPaused as dbSetPaused } from "./db";
import { Mode, SelectedInfo } from "./model";
import { Perf } from "./perf";

export function App() {
  const [mode, setMode] = useState<Mode>("crate");
  const [paused, setPausedState] = useState(false);
  const [hovered, setHovered] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const perf = useRef<Perf>({ fps: 0, worstMs: 0 });
  const { elements, loading } = useGraphData(mode);

  const togglePause = () => {
    const p = !paused;
    setPausedState(p);
    dbSetPaused(p);
  };

  const status = loading ? "connecting" : paused ? "paused" : "live";

  const groups = useMemo(() => {
    const m = new Map<string, string>();
    for (const n of elements.nodes) if (!m.has(n.group)) m.set(n.group, n.color);
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, color]) => ({ name, color }));
  }, [elements.nodes]);

  const info: SelectedInfo | null = useMemo(() => {
    if (!selected) return null;
    const byId = new Map(elements.nodes.map((n) => [n.id, n]));
    const n = byId.get(selected);
    if (!n) return null;
    const imports = elements.edges
      .filter((e) => e.source === selected)
      .map((e) => byId.get(e.target)?.title ?? e.target)
      .sort();
    const importedBy = elements.edges
      .filter((e) => e.target === selected)
      .map((e) => byId.get(e.source)?.title ?? e.source)
      .sort();
    return { id: n.id, label: n.label, title: n.title, group: n.group, kind: n.kind, imports, importedBy };
  }, [selected, elements]);

  return (
    <div className="app">
      <Canvas style={{ position: "absolute", inset: 0 }} gl={{ antialias: true }} flat dpr={[1, 2]}>
        <color attach="background" args={["#0e0e11"]} />
        <ForceGraph
          elements={elements}
          mode={mode}
          hovered={hovered}
          setHovered={setHovered}
          selected={selected}
          setSelected={setSelected}
          perf={perf}
        />
      </Canvas>
      <Hud
        mode={mode}
        setMode={setMode}
        paused={paused}
        togglePause={togglePause}
        status={status}
        counts={{ nodes: elements.nodes.length, edges: elements.edges.length }}
        groups={groups}
        perf={perf}
        info={info}
        onCloseInfo={() => setSelected(null)}
      />
    </div>
  );
}
