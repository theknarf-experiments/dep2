import { useMemo, useRef, useState } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph, GpuForceGraph } from "@dep2/force-graph";
import { Hud } from "./Hud";
import { DataView } from "./DataView";
import { RulesView } from "./RulesView";
import { View } from "./ViewSwitch";
import { useGraphData } from "./useGraphData";
import { setPaused as dbSetPaused } from "./db";
import { Mode, SelectedInfo } from "./model";
import { Perf } from "./perf";

export function App() {
  const [view, setView] = useState<View>("graph");
  // Render the graph on the GPU (WebGPU) by default; fall back to the R3F/WebGL
  // path only when WebGPU is unavailable or init fails.
  const [gpuFailed, setGpuFailed] = useState(false);
  const [mode, setMode] = useState<Mode>("file");
  const [paused, setPausedState] = useState(false);
  const [hovered, setHovered] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [hoverModule, setHoverModule] = useState<string | null>(null);
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

  // The highlighted module: an explicit legend hover wins, otherwise the
  // selected node's module.
  const activeModule = hoverModule ?? (selected ? (info?.group ?? null) : null);

  if (view === "data") {
    return (
      <div className="app">
        <DataView
          view={view}
          setView={setView}
          paused={paused}
          togglePause={togglePause}
          status={status}
        />
      </div>
    );
  }

  if (view === "rules") {
    return (
      <div className="app">
        <RulesView view={view} setView={setView} status={status} />
      </div>
    );
  }

  return (
    <div className="app">
      {gpuFailed ? (
        <Canvas style={{ position: "absolute", inset: 0 }} gl={{ antialias: true }} flat dpr={[1, 2]}>
          <color attach="background" args={["#0e0e11"]} />
          <ForceGraph
            elements={elements}
            layoutKey={mode}
            hovered={hovered}
            setHovered={setHovered}
            selected={selected}
            setSelected={setSelected}
            activeGroup={activeModule}
            perf={perf}
          />
        </Canvas>
      ) : (
        <GpuForceGraph
          elements={elements}
          layoutKey={mode}
          hovered={hovered}
          setHovered={setHovered}
          selected={selected}
          setSelected={setSelected}
          activeGroup={activeModule}
          perf={perf}
          onUnsupported={(reason) => {
            console.warn("[dep2] WebGPU unavailable, using WebGL fallback:", reason);
            setGpuFailed(true);
          }}
        />
      )}
      <Hud
        view={view}
        setView={setView}
        mode={mode}
        setMode={setMode}
        paused={paused}
        togglePause={togglePause}
        status={status}
        counts={{ nodes: elements.nodes.length, edges: elements.edges.length }}
        groups={groups}
        activeModule={activeModule}
        setHoverModule={setHoverModule}
        perf={perf}
        info={info}
        onCloseInfo={() => setSelected(null)}
      />
    </div>
  );
}
