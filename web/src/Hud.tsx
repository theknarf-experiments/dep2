// Plain DOM overlay over the canvas: toolbar, FPS meter, legend, and the
// click-to-select info panel. The container is pointer-transparent so empty
// areas still pan/zoom the graph; interactive surfaces opt back in.

import { MutableRefObject, useEffect, useState } from "react";
import { colorFor, Mode, SelectedInfo } from "./model";
import { Perf } from "./perf";

interface Props {
  mode: Mode;
  setMode: (m: Mode) => void;
  paused: boolean;
  togglePause: () => void;
  status: "connecting" | "live" | "paused";
  counts: { nodes: number; edges: number };
  groups: { name: string; color: string }[];
  activeModule: string | null;
  setHoverModule: (m: string | null) => void;
  perf: MutableRefObject<Perf>;
  info: SelectedInfo | null;
  onCloseInfo: () => void;
}

const LEGEND_LIMIT = 10;

function PerfMeter({ perf }: { perf: MutableRefObject<Perf> }) {
  const [v, setV] = useState<Perf>({ fps: 0, worstMs: 0 });
  useEffect(() => {
    const id = setInterval(() => setV({ ...perf.current }), 400);
    return () => clearInterval(id);
  }, [perf]);
  return (
    <span className="perf" title="frames per second · worst frame time in the last window (stutter)">
      {v.fps} fps <span className={v.worstMs > 24 ? "warn" : "muted"}>· {v.worstMs.toFixed(1)} ms</span>
    </span>
  );
}

function InfoPanel({ info, onClose }: { info: SelectedInfo; onClose: () => void }) {
  const list = (items: string[]) =>
    items.length ? (
      <ul>
        {items.map((s) => (
          <li key={s} title={s}>
            {s}
          </li>
        ))}
      </ul>
    ) : (
      <div className="none">none</div>
    );
  return (
    <div className="info">
      <div className="info-head">
        <span className="info-kind">{info.kind}</span>
        <button className="close" onClick={onClose} aria-label="close">
          ×
        </button>
      </div>
      <div className="info-title">{info.label}</div>
      <dl>
        {info.kind === "file" && (
          <>
            <dt>path</dt>
            <dd>{info.title}</dd>
          </>
        )}
        <dt>module</dt>
        <dd>
          <span className="sw" style={{ background: colorFor(info.group) }} />
          {info.group}
        </dd>
      </dl>
      <div className="info-sec">imports ({info.imports.length})</div>
      {list(info.imports)}
      <div className="info-sec">imported by ({info.importedBy.length})</div>
      {list(info.importedBy)}
    </div>
  );
}

function Legend({
  groups,
  activeModule,
  setHoverModule,
}: {
  groups: { name: string; color: string }[];
  activeModule: string | null;
  setHoverModule: (m: string | null) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const shown = expanded ? groups : groups.slice(0, LEGEND_LIMIT);
  const extra = groups.length - shown.length;
  return (
    <div className="legend" onMouseLeave={() => setHoverModule(null)}>
      {shown.map((g) => (
        <span
          key={g.name}
          className={`chip ${activeModule ? (activeModule === g.name ? "active" : "dim") : ""}`}
          onMouseEnter={() => setHoverModule(g.name)}
        >
          <span className="sw" style={{ background: g.color }} />
          {g.name}
        </span>
      ))}
      {(extra > 0 || expanded) && (
        <button className="legend-more" onClick={() => setExpanded((e) => !e)}>
          {expanded ? "show less" : `+${extra} more`}
        </button>
      )}
    </div>
  );
}

export function Hud({
  mode,
  setMode,
  paused,
  togglePause,
  status,
  counts,
  groups,
  activeModule,
  setHoverModule,
  perf,
  info,
  onCloseInfo,
}: Props) {
  return (
    <div className="hud">
      <div className="bar">
        <span className="brand">dep2</span>
        <span className="sub">live import graph</span>
        <span className="seg">
          <button className={mode === "crate" ? "on" : ""} onClick={() => setMode("crate")}>
            Modules
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
        <PerfMeter perf={perf} />
        <span className={`status ${status}`}>
          <span className="dot" />
          {status}
        </span>
      </div>

      {info && <InfoPanel info={info} onClose={onCloseInfo} />}

      {groups.length > 0 && (
        <Legend groups={groups} activeModule={activeModule} setHoverModule={setHoverModule} />
      )}
    </div>
  );
}
