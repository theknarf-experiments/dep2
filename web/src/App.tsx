import { useMemo, useRef, useState } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph } from "@dep2/force-graph";
import { Hud } from "./Hud";
import { CodeView } from "./CodeView";
import { DataView } from "./DataView";
import { RulesView } from "./RulesView";
import { View } from "./ViewSwitch";
import { useGraphData } from "./useGraphData";
import { setPaused as dbSetPaused } from "./db";
import { applyFilters, EMPTY_FILTERS, Filters, Mode, SelectedInfo } from "./model";
import { resolveView } from "./spec";
import { useVizSpec } from "./useRawData";
import { Perf } from "./perf";

export function App() {
  // The viz spec comes from the ENGINE (GET /spec, the program's sidecar);
  // programs without one get the Data view only.
  const spec = useVizSpec() ?? null;
  const [view, setView] = useState<View>("graph");
  // Jump-to-source target: set by the Data view's "open" buttons; consumed by
  // the Code view (select + expand + scroll to line). A fresh object per
  // click, so re-opening the same location re-triggers the scroll.
  const [codeTarget, setCodeTarget] = useState<{ file: string; line?: number } | null>(null);
  const openInCode = (file: string, line?: number) => {
    setCodeTarget({ file, line });
    setView("code");
  };
  const [mode, setMode] = useState<Mode>("");
  const modes = useMemo(
    () => (spec ? spec.views.map((v) => ({ id: v.id, label: v.label })) : []),
    [spec],
  );
  const effectiveMode = mode || (spec ? spec.defaultView : "");
  const hasGraph = spec !== null;
  const effectiveView = hasGraph ? view : view === "graph" ? "data" : view;
  const [paused, setPausedState] = useState(false);
  const [hovered, setHovered] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [hoverModule, setHoverModule] = useState<string | null>(null);
  const perf = useRef<Perf>({ fps: 0, worstMs: 0 });
  const [filters, setFilters] = useState<Filters>(EMPTY_FILTERS);
  const { elements: unfiltered, classes, loading } = useGraphData(spec, effectiveMode);
  const elements = useMemo(
    () => applyFilters(unfiltered, filters, classes),
    [unfiltered, filters, classes],
  );

  // Edge-kind chips for the current view, labelled by the spec. A view with
  // a single edge relation gets NO chips — there is nothing to mix, and
  // "hide all edges" is not a useful graph.
  const edgeToggles = useMemo(() => {
    if (!spec) return [];
    const view = resolveView(spec, effectiveMode);
    if (view.edges.length < 2) return [];
    return view.edges.map((rel) => ({
      rel,
      label: spec.edges[rel]?.label ?? rel,
      on: !filters.hiddenRels.has(rel),
    }));
  }, [spec, effectiveMode, filters.hiddenRels]);
  const toggleRel = (rel: string) =>
    setFilters((f) => {
      const hiddenRels = new Set(f.hiddenRels);
      if (hiddenRels.has(rel)) hiddenRels.delete(rel);
      else hiddenRels.add(rel);
      return { ...f, hiddenRels };
    });

  // Node-class chips (e.g. "tests"): shown only when the class has members
  // among the view's nodes.
  const classToggles = useMemo(() => {
    const nodeIds = new Set(unfiltered.nodes.map((n) => n.id));
    return (spec?.nodeClasses ?? [])
      .filter((c) => {
        const ids = classes.get(c.rel);
        if (!ids || ids.size === 0) return false;
        for (const id of ids) if (nodeIds.has(id)) return true;
        return false;
      })
      .map((c) => ({
        rel: c.rel,
        label: c.label,
        state: filters.classStates.get(c.rel) ?? null,
      }));
  }, [spec, unfiltered.nodes, classes, filters.classStates]);
  const toggleScope = (group: string) =>
    setFilters((f) => ({ ...f, scopeGroup: f.scopeGroup === group ? null : group }));
  // Class chips cycle neutral -> only (solo) -> hidden -> neutral.
  const toggleClass = (rel: string) =>
    setFilters((f) => {
      const classStates = new Map(f.classStates);
      const cur = classStates.get(rel) ?? null;
      if (cur === null) classStates.set(rel, "only");
      else if (cur === "only") classStates.set(rel, "hidden");
      else classStates.delete(rel);
      return { ...f, classStates };
    });

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
    const ref = (id: string) => ({ id, title: byId.get(id)?.title ?? id });
    const byTitle = (a: { title: string }, b: { title: string }) => a.title.localeCompare(b.title);
    const imports = elements.edges
      .filter((e) => e.source === selected)
      .map((e) => ref(e.target))
      .sort(byTitle);
    const importedBy = elements.edges
      .filter((e) => e.target === selected)
      .map((e) => ref(e.source))
      .sort(byTitle);
    return { id: n.id, label: n.label, title: n.title, group: n.group, kind: n.kind, imports, importedBy };
  }, [selected, elements]);

  // The highlighted module: an explicit legend hover wins, otherwise the
  // selected node's module.
  const activeModule = hoverModule ?? (selected ? (info?.group ?? null) : null);

  if (effectiveView === "data") {
    return (
      <div className="app">
        <DataView
          hasGraph={hasGraph}
          openInCode={openInCode}
          view={effectiveView}
          setView={setView}
          paused={paused}
          togglePause={togglePause}
          status={status}
        />
      </div>
    );
  }

  if (effectiveView === "code") {
    return (
      <div className="app">
        <CodeView
          view={effectiveView}
          setView={setView}
          status={status}
          hasGraph={hasGraph}
          target={codeTarget}
        />
      </div>
    );
  }

  if (effectiveView === "rules") {
    return (
      <div className="app">
        <RulesView view={effectiveView} setView={setView} status={status} hasGraph={hasGraph} />
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
          layoutKey={effectiveMode}
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
        modes={modes}
        mode={effectiveMode}
        setMode={setMode}
        paused={paused}
        togglePause={togglePause}
        status={status}
        counts={{ nodes: elements.nodes.length, edges: elements.edges.length }}
        filters={filters}
        setQuery={(query) => setFilters((f) => ({ ...f, query }))}
        cycleIsolated={() =>
          setFilters((f) => ({
            ...f,
            isolated: f.isolated === null ? "hide" : f.isolated === "hide" ? "only" : null,
          }))
        }
        toggleCrossModule={() =>
          setFilters((f) => ({ ...f, hideCrossModule: !f.hideCrossModule }))
        }
        edgeToggles={edgeToggles}
        toggleRel={toggleRel}
        classToggles={classToggles}
        toggleClass={toggleClass}
        scopeGroup={filters.scopeGroup}
        toggleScope={toggleScope}
        groups={groups}
        activeModule={activeModule}
        setHoverModule={setHoverModule}
        perf={perf}
        info={info}
        onCloseInfo={() => setSelected(null)}
        onHoverNode={setHovered}
        onSelectNode={setSelected}
      />
    </div>
  );
}
