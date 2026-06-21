// Plain DOM overlay over the canvas. Sitting above the <canvas> in the DOM, its
// buttons get native clicks and never reach OrbitControls; the container is
// pointer-transparent so empty areas still pan/zoom the graph.

import { Mode } from "./model";

interface Props {
  mode: Mode;
  setMode: (m: Mode) => void;
  paused: boolean;
  togglePause: () => void;
  status: "connecting" | "live" | "paused";
  counts: { nodes: number; edges: number };
  groups: { name: string; color: string }[];
}

export function Hud({ mode, setMode, paused, togglePause, status, counts, groups }: Props) {
  return (
    <div className="hud">
      <div className="bar">
        <span className="brand">dep2</span>
        <span className="sub">live import graph</span>
        <span className="seg">
          <button className={mode === "crate" ? "on" : ""} onClick={() => setMode("crate")}>
            Crates
          </button>
          <button className={mode === "file" ? "on" : ""} onClick={() => setMode("file")}>
            Files
          </button>
        </span>
        <button className="ghost" onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
        <span className="counts">
          {counts.nodes} nodes · {counts.edges} edges
        </span>
        <span className={`status ${status}`}>
          <span className="dot" />
          {status}
        </span>
      </div>

      {groups.length > 0 && (
        <div className="legend">
          {groups.map((g) => (
            <span key={g.name} className="chip">
              <span className="sw" style={{ background: g.color }} />
              {g.name}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
