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

export interface Program {
  path: string;
  source: string;
}

/** The loaded .dl program (path + source). */
export function useProgram(): Program {
  return usePoll(
    async () => {
      const res = await fetch(`${trimBase(config.api)}/program`);
      if (!res.ok) throw new Error(`program: ${res.status}`);
      return (await res.json()) as Program;
    },
    [],
    { path: "", source: "" },
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
