// Live polling hooks for the raw relation API, used by the Data view. These read
// the same runtime config (api / pollMs / paused) the graph sync uses, so Pause
// and the active engine apply everywhere.
//
//   GET /relations            -> { relations: [{ name, count }, ...] }
//   GET /relations/<name>     -> { name, count, rows: string[][] }

import { useEffect, useRef, useState } from "react";
import { config } from "./db";
import { trimBase } from "./api";

export interface RelInfo {
  name: string;
  count: number;
  /** Declared column names (may be empty for engine-internal relations). */
  columns?: string[];
}

/** Poll `fetcher` immediately and every `pollMs` (skipping while paused). */
function usePoll<T>(fetcher: () => Promise<T>, deps: unknown[], initial: T): T {
  const [value, setValue] = useState<T>(initial);
  const ref = useRef(fetcher);
  ref.current = fetcher;
  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const v = await ref.current();
        if (alive) setValue(v);
      } catch {
        /* keep the last good value on a transient fetch error */
      }
    };
    tick();
    const id = setInterval(() => {
      if (!config.paused) tick();
    }, config.pollMs);
    return () => {
      alive = false;
      clearInterval(id);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return value;
}

/** Every served relation with its current row count. */
export function useRelationList(): RelInfo[] {
  return usePoll(
    async () => {
      const res = await fetch(`${trimBase(config.api)}/relations`);
      if (!res.ok) throw new Error(`relations: ${res.status}`);
      const data = (await res.json()) as { relations?: RelInfo[] };
      return (data.relations ?? []).slice().sort((a, b) => a.name.localeCompare(b.name));
    },
    [],
    [],
  );
}

import type { GraphSpec } from "./spec";

/** The program's sidecar viz spec: undefined while loading, null when the
 * program has none (the UI then hides the graph view). */
export function useVizSpec(): GraphSpec | null | undefined {
  const [spec, setSpec] = useState<GraphSpec | null | undefined>(undefined);
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const tick = async () => {
      try {
        const res = await fetch(`${trimBase(config.api)}/spec`);
        if (!alive) return;
        if (res.ok) {
          setSpec((await res.json()) as GraphSpec);
          return; // loaded once; the spec is static per program
        }
        if (res.status === 404) {
          setSpec(null);
          return;
        }
      } catch {
        /* engine not up yet — retry */
      }
      timer = setTimeout(tick, 1000);
    };
    tick();
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
  }, []);
  return spec;
}

export interface ProgramFile {
  path: string;
  source: string;
}

export interface Program {
  path: string;
  /** Every loaded file (entry first, `.import` closure after). */
  files: ProgramFile[];
  /** Absolute scan roots of the bound sources (for editor links). */
  roots?: string[];
}

/** The loaded .dl program: the entry path + each loaded file's source. */
export function useProgram(): Program {
  return usePoll(
    async () => {
      const res = await fetch(`${trimBase(config.api)}/program`);
      if (!res.ok) throw new Error(`program: ${res.status}`);
      return (await res.json()) as Program;
    },
    [],
    { path: "", files: [], roots: [] },
  );
}

/** The rows of one relation (empty while no relation is selected). */
export function useRelationRows(name: string | null): string[][] {
  return usePoll(
    async () => {
      if (!name) return [];
      const res = await fetch(`${trimBase(config.api)}/relations/${name}`);
      if (!res.ok) throw new Error(`${name}: ${res.status}`);
      const data = (await res.json()) as { rows?: string[][] };
      return data.rows ?? [];
    },
    [name],
    [],
  );
}
