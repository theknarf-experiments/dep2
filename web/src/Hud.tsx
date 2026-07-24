// HUD overlay over the canvas: toolbar, FPS meter, legend, and the
// click-to-select info panel. Styled with a CSS Module (Hud.module.css).

import { MutableRefObject, useEffect, useState } from "react";
import { colorFor, Filters, Mode, NodeRef, SelectedInfo } from "./model";
import { Perf } from "./perf";
import { ViewSwitch, View } from "./ViewSwitch";
import s from "./Hud.module.css";

interface Props {
  view: View;
  setView: (v: View) => void;
  modes: { id: string; label: string }[];
  mode: Mode;
  setMode: (m: Mode) => void;
  paused: boolean;
  togglePause: () => void;
  status: "connecting" | "live" | "paused";
  counts: { nodes: number; edges: number };
  filters: Filters;
  setQuery: (q: string) => void;
  toggleHideIsolated: () => void;
  edgeToggles: { rel: string; label: string; on: boolean }[];
  toggleRel: (rel: string) => void;
  classToggles: { rel: string; label: string; on: boolean }[];
  toggleClass: (rel: string) => void;
  groups: { name: string; color: string }[];
  activeModule: string | null;
  setHoverModule: (m: string | null) => void;
  perf: MutableRefObject<Perf>;
  info: SelectedInfo | null;
  onCloseInfo: () => void;
  /** Hover a neighbor in the info panel -> highlight its node in the graph. */
  onHoverNode: (id: string | null) => void;
  /** Click a neighbor in the info panel -> select (focus) that node. */
  onSelectNode: (id: string) => void;
}

const LEGEND_LIMIT = 6;

/** Live filters: text search (matches keep 1-hop context), per-edge-kind
 * toggles from the spec, and an isolated-node drop. All client-side over the
 * live-polled rows, so the graph updates as you type. */
function FilterBar({
  filters,
  setQuery,
  toggleHideIsolated,
  edgeToggles,
  toggleRel,
  classToggles,
  toggleClass,
}: {
  filters: Filters;
  setQuery: (q: string) => void;
  toggleHideIsolated: () => void;
  edgeToggles: { rel: string; label: string; on: boolean }[];
  toggleRel: (rel: string) => void;
  classToggles: { rel: string; label: string; on: boolean }[];
  toggleClass: (rel: string) => void;
}) {
  return (
    <div className={s.filters} data-testid="filters">
      <input
        className={s.search}
        type="search"
        placeholder="filter nodes…"
        value={filters.query}
        onChange={(e) => setQuery(e.target.value)}
        spellCheck={false}
      />
      {edgeToggles.map((e) => (
        <button
          key={e.rel}
          className={[s.fchip, e.on ? s.fon : s.foff].join(" ")}
          aria-pressed={e.on}
          title={e.on ? `hide ${e.label}` : `show ${e.label}`}
          onClick={() => toggleRel(e.rel)}
        >
          {e.label}
        </button>
      ))}
      {classToggles.map((c) => (
        <button
          key={c.rel}
          className={[s.fchip, c.on ? s.fon : s.foff].join(" ")}
          aria-pressed={c.on}
          title={c.on ? `hide ${c.label}` : `show ${c.label}`}
          onClick={() => toggleClass(c.rel)}
        >
          {c.label}
        </button>
      ))}
      <button
        className={[s.fchip, filters.hideIsolated ? s.fon : s.foff].join(" ")}
        aria-pressed={filters.hideIsolated}
        title="drop nodes with no visible edges"
        onClick={toggleHideIsolated}
      >
        hide isolated
      </button>
    </div>
  );
}

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

function InfoPanel({
  info,
  onClose,
  onHoverNode,
  onSelectNode,
}: {
  info: SelectedInfo;
  onClose: () => void;
  onHoverNode: (id: string | null) => void;
  onSelectNode: (id: string) => void;
}) {
  const list = (items: NodeRef[]) =>
    items.length ? (
      <ul onMouseLeave={() => onHoverNode(null)}>
        {items.map((x) => (
          <li
            key={x.id}
            className={s.neighbor}
            data-testid="neighbor"
            data-node-id={x.id}
            title={x.title}
            onMouseEnter={() => onHoverNode(x.id)}
            onClick={() => {
              onHoverNode(null);
              onSelectNode(x.id);
            }}
          >
            {x.title}
          </li>
        ))}
      </ul>
    ) : (
      <div className={s.none}>none</div>
    );
  return (
    <div className={s.info} data-testid="info" data-node-id={info.id}>
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
  modes,
  mode,
  setMode,
  paused,
  togglePause,
  status,
  counts,
  filters,
  setQuery,
  toggleHideIsolated,
  edgeToggles,
  toggleRel,
  classToggles,
  toggleClass,
  groups,
  activeModule,
  setHoverModule,
  perf,
  info,
  onCloseInfo,
  onHoverNode,
  onSelectNode,
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
          {modes.map((m) => (
            <button
              key={m.id}
              className={mode === m.id ? s.on : undefined}
              aria-pressed={mode === m.id}
              onClick={() => setMode(m.id)}
            >
              {m.label}
            </button>
          ))}
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

      <FilterBar
        filters={filters}
        setQuery={setQuery}
        toggleHideIsolated={toggleHideIsolated}
        edgeToggles={edgeToggles}
        toggleRel={toggleRel}
        classToggles={classToggles}
        toggleClass={toggleClass}
      />

      {info && (
        <InfoPanel
          info={info}
          onClose={onCloseInfo}
          onHoverNode={onHoverNode}
          onSelectNode={onSelectNode}
        />
      )}

      {groups.length > 0 && (
        <Legend groups={groups} activeModule={activeModule} setHoverModule={setHoverModule} />
      )}
    </div>
  );
}
