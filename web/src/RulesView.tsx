// Rules view: shows the .dl program loaded into the engine (fetched from
// /program), with light per-line highlighting, a rule/declaration summary, and
// a find box that highlights matches.

import { Fragment, useMemo, useState } from "react";
import { useProgram } from "./useRawData";
import { ViewSwitch, View } from "./ViewSwitch";
import { tokenizeLine } from "./dlHighlight";
import s from "./RulesView.module.css";

interface Props {
  view: View;
  setView: (v: View) => void;
  status: "connecting" | "live" | "paused";
}

/** Split token text into plain runs and <mark>ed matches of `q`. */
function highlight(text: string, q: string, keyBase: string) {
  if (!q) return text;
  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const out: React.ReactNode[] = [];
  let i = 0;
  let n = 0;
  for (;;) {
    const at = lower.indexOf(needle, i);
    if (at === -1) {
      out.push(text.slice(i));
      break;
    }
    if (at > i) out.push(text.slice(i, at));
    out.push(
      <mark key={`${keyBase}-${n++}`} className={s.match}>
        {text.slice(at, at + needle.length)}
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
          <div key={i} className={s.line}>
            <span className={s.gutter}>{i + 1}</span>
            <span className={s.text}>
              {line === "" ? (
                <Fragment>&nbsp;</Fragment>
              ) : (
                tokenizeLine(line).map((tok, j) => (
                  <span key={j} className={s[tok.cls]}>
                    {highlight(tok.text, query, `${i}-${j}`)}
                  </span>
                ))
              )}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
