import { useEffect, useMemo, useRef, useState } from "react";
import { DEFAULT_API, fetchRelations } from "./api";
import { buildElements, Mode, RawRelations } from "./model";
import { Graph } from "./Graph";

const RELATIONS = ["crate_node", "crate_edge", "file_node", "file_edge"];
const EMPTY: RawRelations = { crate_node: [], crate_edge: [], file_node: [], file_edge: [] };

type Status = "connecting" | "live" | "error";

export function App() {
  const [api, setApi] = useState(DEFAULT_API);
  const [mode, setMode] = useState<Mode>("crate");
  const [pollMs, setPollMs] = useState(1500);
  const [paused, setPaused] = useState(false);
  const [rels, setRels] = useState<RawRelations>(EMPTY);
  const [status, setStatus] = useState<Status>("connecting");
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);

  // Avoid setState after unmount during in-flight fetches.
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  useEffect(() => {
    if (paused) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const r = await fetchRelations(api, RELATIONS);
        if (cancelled || !alive.current) return;
        setRels(r as unknown as RawRelations);
        setStatus("live");
        setError(null);
        setUpdatedAt(Date.now());
      } catch (e) {
        if (cancelled || !alive.current) return;
        setStatus("error");
        setError(e instanceof Error ? e.message : String(e));
      }
    };
    setStatus((s) => (s === "live" ? s : "connecting"));
    tick();
    const id = setInterval(tick, pollMs);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, [api, pollMs, paused]);

  const elements = useMemo(() => buildElements(mode, rels), [mode, rels]);

  return (
    <div className="app">
      <header className="bar">
        <div className="brand">
          dep2 <span className="brand-sub">live import graph</span>
        </div>

        <div className="seg">
          <button
            className={mode === "crate" ? "on" : ""}
            onClick={() => setMode("crate")}
          >
            Crates
          </button>
          <button
            className={mode === "file" ? "on" : ""}
            onClick={() => setMode("file")}
          >
            Files
          </button>
        </div>

        <div className="counts">
          {elements.nodes.length} nodes · {elements.edges.length} edges
        </div>

        <div className="spacer" />

        <label className="field">
          API
          <input value={api} onChange={(e) => setApi(e.target.value.trim())} spellCheck={false} />
        </label>
        <label className="field">
          every
          <input
            className="num"
            type="number"
            min={250}
            step={250}
            value={pollMs}
            onChange={(e) => setPollMs(Math.max(250, Number(e.target.value) || 1500))}
          />
          ms
        </label>
        <button className="ghost" onClick={() => setPaused((p) => !p)}>
          {paused ? "Resume" : "Pause"}
        </button>

        <div className={`status ${status}`} title={error ?? ""}>
          <span className="dot" />
          {status === "live"
            ? paused
              ? "paused"
              : "live"
            : status === "connecting"
              ? "connecting…"
              : "error"}
        </div>
      </header>

      {status === "error" && (
        <div className="banner">
          Can’t reach <code>{api}</code> — {error}. Is the engine running (
          <code>mise run graph</code>)?
        </div>
      )}

      <Graph elements={elements} mode={mode} />

      <footer className="foot">
        {updatedAt ? `updated ${new Date(updatedAt).toLocaleTimeString()}` : "—"} · click a node
        to focus its neighborhood · scroll to zoom, drag to pan
      </footer>
    </div>
  );
}
