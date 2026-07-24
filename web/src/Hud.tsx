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
  cycleIsolated: () => void;
  toggleCrossModule: () => void;
  edgeToggles: { rel: string; label: string; on: boolean }[];
  toggleRel: (rel: string) => void;
  classToggles: { rel: string; label: string; state: "only" | "hidden" | null }[];
  toggleClass: (rel: string) => void;
  scopeGroup: string | null;
  toggleScope: (group: string) => void;
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
  cycleIsolated,
  toggleCrossModule,
  edgeToggles,
  toggleRel,
  classToggles,
  toggleClass,
  scope,
  clearScope,
}: {
  filters: Filters;
  setQuery: (q: string) => void;
  cycleIsolated: () => void;
  toggleCrossModule: () => void;
  edgeToggles: { rel: string; label: string; on: boolean }[];
  toggleRel: (rel: string) => void;
  classToggles: { rel: string; label: string; state: "only" | "hidden" | null }[];
  toggleClass: (rel: string) => void;
  scope: string | null;
  clearScope: () => void;
}) {
  return (
    <div className={s.filters} data-testid="filters">
      {scope && (
        <button
          className={[s.fchip, s.fon].join(" ")}
          title="clear the module scope"
          onClick={clearScope}
        >
          scoped: {scope} ✕
        </button>
      )}
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
          className={[
            s.fchip,
            c.state === "only" ? s.fsolo : c.state === "hidden" ? s.foff : "",
          ]
            .filter(Boolean)
            .join(" ")}
          title={
            c.state === null
              ? `show only ${c.label}`
              : c.state === "only"
                ? `hide ${c.label}`
                : `reset ${c.label}`
          }
          onClick={() => toggleClass(c.rel)}
        >
          {c.state === "only" ? `only: ${c.label}` : c.label}
        </button>
      ))}
      <button
        className={[
          s.fchip,
          filters.isolated === "only" ? s.fsolo : filters.isolated === "hide" ? s.foff : "",
        ]
          .filter(Boolean)
          .join(" ")}
        title={
          filters.isolated === null
            ? "hide nodes with no visible edges"
            : filters.isolated === "hide"
              ? "show ONLY isolated nodes (orphans)"
              : "reset"
        }
        onClick={cycleIsolated}
      >
        {filters.isolated === "only" ? "only: isolated" : "isolated"}
      </button>
      <button
        className={[s.fchip, filters.hideCrossModule ? s.foff : ""].filter(Boolean).join(" ")}
        aria-pressed={!filters.hideCrossModule}
        title={
          filters.hideCrossModule
            ? "show edges between different modules"
            : "hide edges between different modules"
        }
        onClick={toggleCrossModule}
      >
        cross-module
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
  scopeGroup,
  toggleScope,
}: {
  groups: { name: string; color: string }[];
  activeModule: string | null;
  setHoverModule: (m: string | null) => void;
  scopeGroup: string | null;
  toggleScope: (group: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const shown = expanded ? groups : groups.slice(0, LEGEND_LIMIT);
  const extra = groups.length - shown.length;
  return (
    <div className={s.legend} onMouseLeave={() => setHoverModule(null)}>
      {shown.map((g) => {
        const cls = [
          s.chip,
          scopeGroup === g.name ? s.scoped : "",
          activeModule ? (activeModule === g.name ? s.active : s.dim) : "",
        ]
          .filter(Boolean)
          .join(" ");
        return (
          <span
            key={g.name}
            className={cls}
            title={scopeGroup === g.name ? "click to unscope" : "click to scope the graph to this module"}
            onMouseEnter={() => setHoverModule(g.name)}
            onClick={() => toggleScope(g.name)}
          >
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
  cycleIsolated,
  toggleCrossModule,
  edgeToggles,
  toggleRel,
  classToggles,
  toggleClass,
  scopeGroup,
  toggleScope,
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
        cycleIsolated={cycleIsolated}
        toggleCrossModule={toggleCrossModule}
        edgeToggles={edgeToggles}
        toggleRel={toggleRel}
        classToggles={classToggles}
        toggleClass={toggleClass}
        scope={scopeGroup}
        clearScope={() => scopeGroup && toggleScope(scopeGroup)}
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
        <Legend
          groups={groups}
          activeModule={activeModule}
          setHoverModule={setHoverModule}
          scopeGroup={scopeGroup}
          toggleScope={toggleScope}
        />
      )}
    </div>
  );
}
