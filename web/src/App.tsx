import { useMemo, useRef, useState } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph } from "@dep2/force-graph";
import { Hud } from "./Hud";
import { DataView } from "./DataView";
import { RulesView } from "./RulesView";
import { View } from "./ViewSwitch";
import { useGraphData } from "./useGraphData";
import { setPaused as dbSetPaused } from "./db";
import { Mode, SelectedInfo } from "./model";
import { IMPORT_GRAPH_SPEC } from "./spec";
import { Perf } from "./perf";

// Graph view options come from the spec, so the HUD toggle reflects whatever
// views the analysis defines.
const MODES = IMPORT_GRAPH_SPEC.views.map((v) => ({ id: v.id, label: v.label }));

export function App() {
  const [view, setView] = useState<View>("graph");
  const [mode, setMode] = useState<Mode>(IMPORT_GRAPH_SPEC.defaultView);
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
      {/* R3F renders + handles all interaction; the force layout runs on the GPU
          (WebGPU) when available and falls back to the d3-force worker otherwise. */}
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
      <Hud
        view={view}
        setView={setView}
        modes={MODES}
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
