// HUD overlay over the canvas: toolbar, FPS meter, legend, and the
// click-to-select info panel. Styled with a CSS Module (Hud.module.css).

import { MutableRefObject, useEffect, useState } from "react";
import { colorFor, Mode, SelectedInfo } from "./model";
import { Perf } from "./perf";
import { ViewSwitch, View } from "./ViewSwitch";
import s from "./Hud.module.css";

interface Props {
  view: View;
  setView: (v: View) => void;
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

const LEGEND_LIMIT = 6;

function PerfMeter({ perf }: { perf: MutableRefObject<Perf> }) {
  const [v, setV] = useState<Perf>({ fps: 0, worstMs: 0 });
  useEffect(() => {
    const id = setInterval(() => setV({ ...perf.current }), 400);
    return () => clearInterval(id);
  }, [perf]);
  return (
    <span
      className={s.perf}
      data-testid="perf"
      title="frames per second · worst frame time in the last window (stutter)"
    >
      {v.fps} fps <span className={v.worstMs > 24 ? s.warn : s.muted}>· {v.worstMs.toFixed(1)} ms</span>
    </span>
  );
}

function InfoPanel({ info, onClose }: { info: SelectedInfo; onClose: () => void }) {
  const list = (items: string[]) =>
    items.length ? (
      <ul>
        {items.map((x) => (
          <li key={x} title={x}>
            {x}
          </li>
        ))}
      </ul>
    ) : (
      <div className={s.none}>none</div>
    );
  return (
    <div className={s.info} data-testid="info">
      <div className={s.infoHead}>
        <span className={s.infoKind}>{info.kind}</span>
        <button className={s.close} onClick={onClose} aria-label="close">
          ×
        </button>
      </div>
      <div className={s.infoTitle}>{info.label}</div>
      <dl>
        {info.kind === "file" && (
          <>
            <dt>path</dt>
            <dd>{info.title}</dd>
          </>
        )}
        <dt>module</dt>
        <dd>
          <span className={s.sw} style={{ background: colorFor(info.group) }} />
          {info.group}
        </dd>
      </dl>
      <div className={s.infoSec}>imports ({info.imports.length})</div>
      {list(info.imports)}
      <div className={s.infoSec}>imported by ({info.importedBy.length})</div>
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
    <div className={s.legend} onMouseLeave={() => setHoverModule(null)}>
      {shown.map((g) => {
        const cls = [s.chip, activeModule ? (activeModule === g.name ? s.active : s.dim) : ""]
          .filter(Boolean)
          .join(" ");
        return (
          <span key={g.name} className={cls} onMouseEnter={() => setHoverModule(g.name)}>
            <span className={s.sw} style={{ background: g.color }} />
            {g.name}
          </span>
        );
      })}
      {(extra > 0 || expanded) && (
        <button className={s.legendMore} onClick={() => setExpanded((e) => !e)}>
          {expanded ? "show less" : `+${extra} more`}
        </button>
      )}
    </div>
  );
}

export function Hud({
  view,
  setView,
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
  const statusCls = [s.status, status === "live" ? s.live : status === "connecting" ? s.connecting : ""]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={s.hud}>
      <div className={s.bar}>
        <span className={s.brand}>dep2</span>
        <ViewSwitch view={view} setView={setView} />
        <span className={s.seg}>
          <button
            className={mode === "crate" ? s.on : undefined}
            aria-pressed={mode === "crate"}
            onClick={() => setMode("crate")}
          >
            Modules
          </button>
          <button
            className={mode === "file" ? s.on : undefined}
            aria-pressed={mode === "file"}
            onClick={() => setMode("file")}
          >
            Files
          </button>
        </span>
        <button className={s.ghost} onClick={togglePause}>
          {paused ? "Resume" : "Pause"}
        </button>
        <span className={s.counts} data-testid="counts">
          {counts.nodes} nodes · {counts.edges} edges
        </span>
        <PerfMeter perf={perf} />
        <span className={statusCls} data-testid="status">
          <span className={s.dot} />
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
