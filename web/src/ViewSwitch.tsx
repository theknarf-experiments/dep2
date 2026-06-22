// Top-level Graph / Data view toggle, shared by the graph HUD and the Data view.

import s from "./ViewSwitch.module.css";

export type View = "graph" | "data";

export function ViewSwitch({ view, setView }: { view: View; setView: (v: View) => void }) {
  return (
    <span className={s.seg}>
      <button
        className={view === "graph" ? s.on : undefined}
        aria-pressed={view === "graph"}
        onClick={() => setView("graph")}
      >
        Graph
      </button>
      <button
        className={view === "data" ? s.on : undefined}
        aria-pressed={view === "data"}
        onClick={() => setView("data")}
      >
        Data
      </button>
    </span>
  );
}
