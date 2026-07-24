// Top-level view toggle, shared by the graph HUD, the Data view, and the Rules
// view.

import s from "./ViewSwitch.module.css";

export type View = "graph" | "data" | "code" | "rules";

const VIEWS: { id: View; label: string }[] = [
  { id: "graph", label: "Graph" },
  { id: "data", label: "Data" },
  { id: "code", label: "Code" },
  { id: "rules", label: "Rules" },
];

export function ViewSwitch({
  view,
  setView,
  hasGraph = true,
}: {
  view: View;
  setView: (v: View) => void;
  /** Hide the Graph tab when the program has no viz spec. */
  hasGraph?: boolean;
}) {
  return (
    <span className={s.seg}>
      {VIEWS.filter((v) => hasGraph || v.id !== "graph").map((v) => (
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
