// Code view: the scanned repo's files in a collapsible tree (left) and the
// selected file's source, line-numbered and syntax-highlighted (right).
// Contents come from GET /files and GET /file/<rel> — server-side walks of
// the bound sources' roots with the scanners' ignore rules.

import { useEffect, useMemo, useState } from "react";
import hljs from "highlight.js/lib/core";
import rust from "highlight.js/lib/languages/rust";
import typescript from "highlight.js/lib/languages/typescript";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import xml from "highlight.js/lib/languages/xml";
import css from "highlight.js/lib/languages/css";
import ini from "highlight.js/lib/languages/ini";
import markdown from "highlight.js/lib/languages/markdown";
import { config } from "./db";
import { trimBase } from "./api";
import { ViewSwitch, View } from "./ViewSwitch";
import s from "./CodeView.module.css";

hljs.registerLanguage("rust", rust);
hljs.registerLanguage("typescript", typescript);
hljs.registerLanguage("javascript", javascript);
hljs.registerLanguage("json", json);
hljs.registerLanguage("xml", xml);
hljs.registerLanguage("css", css);
hljs.registerLanguage("ini", ini);
hljs.registerLanguage("markdown", markdown);

const LANG_BY_EXT: Record<string, string> = {
  rs: "rust",
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  json: "json",
  html: "xml",
  htm: "xml",
  css: "css",
  toml: "ini",
  md: "markdown",
  markdown: "markdown",
};

interface Props {
  view: View;
  setView: (v: View) => void;
  status: "connecting" | "live" | "paused";
  hasGraph?: boolean;
}

/** Nested directory structure built from the flat relative-path list. */
interface Dir {
  dirs: Map<string, Dir>;
  files: string[]; // full relative paths
}

function buildTree(paths: string[]): Dir {
  const root: Dir = { dirs: new Map(), files: [] };
  for (const p of paths) {
    const parts = p.split("/");
    let d = root;
    for (const part of parts.slice(0, -1)) {
      let next = d.dirs.get(part);
      if (!next) {
        next = { dirs: new Map(), files: [] };
        d.dirs.set(part, next);
      }
      d = next;
    }
    d.files.push(p);
  }
  return root;
}

function TreeDir({
  name,
  dir,
  depth,
  selected,
  onSelect,
  openDirs,
  toggleDir,
  path,
}: {
  name: string;
  dir: Dir;
  depth: number;
  selected: string | null;
  onSelect: (p: string) => void;
  openDirs: Set<string>;
  toggleDir: (p: string) => void;
  path: string;
}) {
  const open = depth === 0 || openDirs.has(path);
  return (
    <div>
      {depth > 0 && (
        <button
          className={s.dir}
          style={{ paddingLeft: 8 + (depth - 1) * 12 }}
          onClick={() => toggleDir(path)}
        >
          <span className={s.arrow}>{open ? "▾" : "▸"}</span>
          {name}
        </button>
      )}
      {open && (
        <>
          {[...dir.dirs.entries()].map(([n, d]) => (
            <TreeDir
              key={n}
              name={n}
              dir={d}
              depth={depth + 1}
              selected={selected}
              onSelect={onSelect}
              openDirs={openDirs}
              toggleDir={toggleDir}
              path={path ? `${path}/${n}` : n}
            />
          ))}
          {dir.files.map((f) => {
            const base = f.split("/").pop();
            return (
              <button
                key={f}
                className={f === selected ? s.fileOn : s.fileOff}
                style={{ paddingLeft: 20 + Math.max(0, depth - 0) * 12 }}
                title={f}
                onClick={() => onSelect(f)}
              >
                {base}
              </button>
            );
          })}
        </>
      )}
    </div>
  );
}

export function CodeView({ view, setView, status, hasGraph }: Props) {
  const [files, setFiles] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [openDirs, setOpenDirs] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const res = await fetch(`${trimBase(config.api)}/files`);
        if (!res.ok) return;
        const data = (await res.json()) as { files?: string[] };
        if (alive) setFiles(data.files ?? []);
      } catch {
        /* engine not up yet; the status pill already says connecting */
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (!selected) return;
    let alive = true;
    (async () => {
      try {
        const res = await fetch(`${trimBase(config.api)}/file/${selected}`);
        const data = (await res.json()) as { content?: string; error?: string };
        if (!alive) return;
        if (res.ok) {
          setContent(data.content ?? "");
          setError(null);
        } else {
          setContent("");
          setError(data.error ?? `HTTP ${res.status}`);
        }
      } catch (e) {
        if (alive) setError(String(e));
      }
    })();
    return () => {
      alive = false;
    };
  }, [selected]);

  const shownFiles = useMemo(() => {
    if (!filter) return files;
    const q = filter.toLowerCase();
    return files.filter((f) => f.toLowerCase().includes(q));
  }, [files, filter]);
  const tree = useMemo(() => buildTree(shownFiles), [shownFiles]);
  // While filtering, open everything so matches are visible.
  const effectiveOpen = useMemo(() => {
    if (!filter) return openDirs;
    const all = new Set<string>();
    for (const f of shownFiles) {
      const parts = f.split("/");
      let p = "";
      for (const part of parts.slice(0, -1)) {
        p = p ? `${p}/${part}` : part;
        all.add(p);
      }
    }
    return all;
  }, [filter, shownFiles, openDirs]);
  const toggleDir = (p: string) =>
    setOpenDirs((prev) => {
      const next = new Set(prev);
      if (next.has(p)) next.delete(p);
      else next.add(p);
      return next;
    });

  const highlighted = useMemo(() => {
    if (!content) return [];
    const ext = selected?.split(".").pop()?.toLowerCase() ?? "";
    const lang = LANG_BY_EXT[ext];
    const html = lang
      ? hljs.highlight(content, { language: lang }).value
      : hljs.highlightAuto(content).value;
    return html.split("\n");
  }, [content, selected]);

  const statusCls = [s.status, status === "live" ? s.live : status === "connecting" ? s.connecting : ""]
    .filter(Boolean)
    .join(" ");

  return (
    <div className={s.wrap}>
      <div className={s.bar}>
        <span className={s.brand}>dep2</span>
        <ViewSwitch view={view} setView={setView} hasGraph={hasGraph} />
        <input
          className={s.filter}
          placeholder="Filter files…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data-testid="code-filter"
        />
        <span className={s.counts}>{files.length} files</span>
        <span className={statusCls}>
          <span className={s.dot} />
          {status}
        </span>
      </div>

      <div className={s.body}>
        <nav className={s.tree} data-testid="code-tree">
          <TreeDir
            name=""
            dir={tree}
            depth={0}
            selected={selected}
            onSelect={setSelected}
            openDirs={effectiveOpen}
            toggleDir={toggleDir}
            path=""
          />
        </nav>

        <div className={s.code} data-testid="code-source">
          {error && <div className={s.error}>{error}</div>}
          {!error && !selected && <div className={s.hint}>Select a file</div>}
          {!error &&
            selected &&
            highlighted.map((lineHtml, i) => (
              <div key={i} className={s.line}>
                <span className={s.gutter}>{i + 1}</span>
                <span
                  className={s.text}
                  // highlight.js output is escaped HTML spans — safe to inject.
                  dangerouslySetInnerHTML={{ __html: lineHtml || "&nbsp;" }}
                />
              </div>
            ))}
        </div>
      </div>
    </div>
  );
}
