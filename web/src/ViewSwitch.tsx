// Top-level view toggle, shared by the graph HUD, the Data view, and the Rules
// view.

import s from "./ViewSwitch.module.css";

export type View = "graph" | "data" | "rules";

const VIEWS: { id: View; label: string }[] = [
  { id: "graph", label: "Graph" },
  { id: "data", label: "Data" },
  { id: "rules", label: "Rules" },
];

export function ViewSwitch({ view, setView }: { view: View; setView: (v: View) => void }) {
  return (
    <span className={s.seg}>
      {VIEWS.map((v) => (
        <button
          key={v.id}
          className={view === v.id ? s.on : undefined}
          aria-pressed={view === v.id}
          onClick={() => setView(v.id)}
        >
          {v.label}
        </button>
      ))}
    </span>
  );
}
