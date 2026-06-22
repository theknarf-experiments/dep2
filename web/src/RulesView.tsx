// Rules view: shows the .dl program loaded into the engine (fetched from
// /program), with light per-line highlighting, a rule/declaration summary, and
// a find box that highlights matches.

import { Fragment, useMemo, useState } from "react";
import { useProgram } from "./useRawData";
import { ViewSwitch, View } from "./ViewSwitch";
import s from "./RulesView.module.css";

interface Props {
  view: View;
  setView: (v: View) => void;
  status: "connecting" | "live" | "paused";
}

type LineKind = "comment" | "directive" | "code" | "blank";

function lineKind(line: string): LineKind {
  const t = line.trim();
  if (t === "") return "blank";
  if (t.startsWith("//")) return "comment";
  if (t.startsWith(".")) return "directive";
  return "code";
}

/** Split a line into plain text and <mark>ed matches of `q` (case-insensitive). */
function highlight(line: string, q: string) {
  if (!q) return line;
  const lower = line.toLowerCase();
  const needle = q.toLowerCase();
  const out: React.ReactNode[] = [];
  let i = 0;
  let n = 0;
  for (;;) {
    const at = lower.indexOf(needle, i);
    if (at === -1) {
      out.push(line.slice(i));
      break;
    }
    if (at > i) out.push(line.slice(i, at));
    out.push(
      <mark key={n++} className={s.match}>
        {line.slice(at, at + needle.length)}
      </mark>,
    );
    i = at + needle.length;
  }
  return out;
}

export function RulesView({ view, setView, status }: Props) {
  const program = useProgram();
  const [query, setQuery] = useState("");

  const lines = useMemo(() => program.source.split("\n"), [program.source]);
  const stats = useMemo(() => {
    const rules = (program.source.match(/:-/g) ?? []).length;
    const decls = (program.source.match(/^\s*\.decl\b/gm) ?? []).length;
    return { rules, decls, lines: lines.length };
  }, [program.source, lines.length]);
  const matches = useMemo(() => {
    if (!query) return 0;
    return lines.reduce(
      (acc, l) => acc + (l.toLowerCase().includes(query.toLowerCase()) ? 1 : 0),
      0,
    );
  }, [lines, query]);

  const statusCls = [s.status, status === "live" ? s.live : status === "connecting" ? s.connecting : ""]
    .filter(Boolean)
    .join(" ");
  const file = program.path ? program.path.split("/").pop() : "";

  return (
    <div className={s.wrap}>
      <div className={s.bar}>
        <span className={s.brand}>dep2</span>
        <ViewSwitch view={view} setView={setView} />
        <span className={s.file} title={program.path}>
          {file}
        </span>
        <input
          className={s.filter}
          placeholder="Find in rules…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          data-testid="rules-find"
        />
        <span className={s.counts} data-testid="rules-stats">
          {stats.rules} rules · {stats.decls} decls{query ? ` · ${matches} lines match` : ""}
        </span>
        <span className={statusCls}>
          <span className={s.dot} />
          {status}
        </span>
      </div>

      <div className={s.code} data-testid="rules-source">
        {lines.map((line, i) => (
          <div key={i} className={`${s.line} ${s[lineKind(line)]}`}>
            <span className={s.gutter}>{i + 1}</span>
            <span className={s.text}>
              {/* keep blank lines from collapsing */}
              {line === "" ? <Fragment>&nbsp;</Fragment> : highlight(line, query)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
