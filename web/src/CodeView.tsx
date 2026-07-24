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
import { collectionFor, config } from "./db";
import { trimBase } from "./api";
import { colorFor } from "./model";
import { useRelationList } from "./useRawData";
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
  /** Jump-to-source target from other views: open this file, scroll to and
   *  highlight the line. A fresh object per navigation re-triggers. */
  target?: { file: string; line?: number } | null;
  /** Resolve a file to its graph node id (spec sourcePath inverse); enables
   *  the "show in graph" button. */
  graphNodeForFile?: (file: string) => string | null;
  /** Navigate to the graph with a node selected. */
  onShowInGraph?: (file: string) => void;
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

export function CodeView({
  view,
  setView,
  status,
  hasGraph,
  target,
  graphNodeForFile,
  onShowInGraph,
}: Props) {
  const [files, setFiles] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [openDirs, setOpenDirs] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");
  const [hitLine, setHitLine] = useState<number | null>(null);
  // Marker sources the user switched OFF (all file+line relations default on).
  const [markersOff, setMarkersOff] = useState<Set<string>>(new Set());
  const [markerRows, setMarkerRows] = useState<Record<string, string[][]>>({});
  const relations = useRelationList();

  // Relations eligible as inline annotations: they declare BOTH a file and a
  // line column. Purely conventional — any analysis gains markers by naming
  // its columns file/line.
  const markerRels = useMemo(
    () =>
      relations
        .filter((r) => {
          const cols = r.columns ?? [];
          return (cols.includes("file") || cols.includes("path")) && cols.includes("line");
        })
        .map((r) => ({
          name: r.name,
          fileCol: (r.columns ?? []).findIndex((c) => c === "file" || c === "path"),
          lineCol: (r.columns ?? []).indexOf("line"),
        })),
    [relations],
  );

  // Live rows for each marker relation via the shared polling collections.
  useEffect(() => {
    if (markerRels.length === 0) return;
    const read = () => {
      const out: Record<string, string[][]> = {};
      for (const m of markerRels) {
        out[m.name] = collectionFor(m.name).toArray.map((row) => row.cols);
      }
      setMarkerRows(out);
    };
    read();
    const subs = markerRels.map((m) => {
      const c = collectionFor(m.name);
      void c.preload();
      return c.subscribeChanges(read);
    });
    return () => subs.forEach((s) => s.unsubscribe());
  }, [markerRels]);

  // line -> markers for the SELECTED file.
  const markersByLine = useMemo(() => {
    const out = new Map<number, { rel: string; row: string[] }[]>();
    if (!selected) return out;
    for (const m of markerRels) {
      if (markersOff.has(m.name)) continue;
      for (const row of markerRows[m.name] ?? []) {
        if (row[m.fileCol] !== selected) continue;
        const line = parseInt(row[m.lineCol], 10);
        if (!line) continue;
        const list = out.get(line) ?? [];
        list.push({ rel: m.name, row });
        out.set(line, list);
      }
    }
    return out;
  }, [selected, markerRels, markerRows, markersOff]);

  // Marker legend entries with per-file counts.
  const markerLegend = useMemo(
    () =>
      markerRels
        .map((m) => ({
          name: m.name,
          count: (markerRows[m.name] ?? []).filter((row) => row[m.fileCol] === selected).length,
          on: !markersOff.has(m.name),
        }))
        .filter((m) => m.count > 0),
    [markerRels, markerRows, selected, markersOff],
  );
  const toggleMarker = (name: string) =>
    setMarkersOff((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });

  // Consume a jump-to-source target: select the file, expand its ancestor
  // directories, and remember the line to scroll to once content renders.
  useEffect(() => {
    if (!target) return;
    setSelected(target.file);
    setHitLine(target.line ?? null);
    setOpenDirs((prev) => {
      const next = new Set(prev);
      const parts = target.file.split("/");
      let p = "";
      for (const part of parts.slice(0, -1)) {
        p = p ? `${p}/${part}` : part;
        next.add(p);
      }
      return next;
    });
  }, [target]);

  // Live: both the tree and the open file's content poll while the view is
  // mounted (the engine watches the repo, so edits show up here too).
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const res = await fetch(`${trimBase(config.api)}/files`);
        if (!res.ok) return;
        const data = (await res.json()) as { files?: string[] };
        if (alive) {
          setFiles((prev) => {
            const next = data.files ?? [];
            return prev.length === next.length && prev.every((p, i) => p === next[i])
              ? prev
              : next;
          });
        }
      } catch {
        /* engine not up yet; the status pill already says connecting */
      }
    };
    tick();
    const id = setInterval(() => {
      if (!config.paused) tick();
    }, Math.max(config.pollMs, 2000));
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  useEffect(() => {
    if (!selected) return;
    let alive = true;
    const tick = async () => {
      try {
        const res = await fetch(`${trimBase(config.api)}/file/${selected}`);
        const data = (await res.json()) as { content?: string; error?: string };
        if (!alive) return;
        if (res.ok) {
          // Only update on real change so scrolling isn't disturbed.
          setContent((prev) => (prev === (data.content ?? "") ? prev : (data.content ?? "")));
          setError(null);
        } else {
          setContent("");
          setError(data.error ?? `HTTP ${res.status}`);
        }
      } catch (e) {
        if (alive) setError(String(e));
      }
    };
    tick();
    const id = setInterval(() => {
      if (!config.paused) tick();
    }, Math.max(config.pollMs, 2000));
    return () => {
      alive = false;
      clearInterval(id);
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
  const selectFile = (p: string) => {
    setSelected(p);
    setHitLine(null);
  };
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

  // Deep link: the current position is always in the URL hash, so positions
  // are shareable/bookmarkable (#code/<file>:<line>). replaceState avoids
  // history spam while browsing.
  useEffect(() => {
    if (!selected) return;
    const hash = `#code/${encodeURIComponent(selected)}${hitLine ? `:${hitLine}` : ""}`;
    history.replaceState(null, "", hash);
  }, [selected, hitLine]);

  // Scroll the target line into view once the highlighted content is in the
  // DOM (content load is async).
  useEffect(() => {
    if (hitLine === null || highlighted.length === 0) return;
    const el = document.querySelector(`[data-line="${hitLine}"]`);
    if (el) el.scrollIntoView({ block: "center" });
  }, [hitLine, highlighted]);

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
        {selected && graphNodeForFile?.(selected) && onShowInGraph && (
          <button
            className={s.graphBtn}
            data-testid="code-show-in-graph"
            title="select this file's node in the graph"
            onClick={() => onShowInGraph(selected)}
          >
            show in graph
          </button>
        )}
        {markerLegend.map((m) => (
          <button
            key={m.name}
            className={[s.markerChip, m.on ? "" : s.markerOff].filter(Boolean).join(" ")}
            title={`${m.on ? "hide" : "show"} ${m.name} markers`}
            onClick={() => toggleMarker(m.name)}
          >
            <span className={s.markerDot} style={{ background: colorFor(m.name) }} />
            {m.name} ({m.count})
          </button>
        ))}
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
            onSelect={selectFile}
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
            highlighted.map((lineHtml, i) => {
              const marks = markersByLine.get(i + 1);
              const cls = [
                s.line,
                i + 1 === hitLine ? s.lineHit : "",
                marks ? s.lineMarked : "",
              ]
                .filter(Boolean)
                .join(" ");
              return (
                <div
                  key={i}
                  className={cls}
                  data-line={i + 1}
                  title={marks?.map((m) => `${m.rel}(${m.row.join(", ")})`).join("\n")}
                >
                  <span className={s.marks}>
                    {(marks ?? []).slice(0, 3).map((m, j) => (
                      <span
                        key={j}
                        className={s.markerDot}
                        style={{ background: colorFor(m.rel) }}
                      />
                    ))}
                  </span>
                  <span className={s.gutter}>{i + 1}</span>
                  <span
                    className={s.text}
                    // highlight.js output is escaped HTML spans — safe to inject.
                    dangerouslySetInnerHTML={{ __html: lineHtml || "&nbsp;" }}
                  />
                </div>
              );
            })}
        </div>
      </div>
    </div>
  );
}
