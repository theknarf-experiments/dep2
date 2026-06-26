// TanStack DB is the reactive store. Each engine relation is a collection of
// rows; a TanStack Query-backed sync polls the dep2 HTTP API and diffs rows into
// the collection by key, so live queries (see useGraphData) update incrementally
// as the engine recomputes — mirroring the engine's own incremental model.

import { QueryClient } from "@tanstack/query-core";
import { createCollection } from "@tanstack/react-db";
import { queryCollectionOptions } from "@tanstack/query-db-collection";
import { DEFAULT_API, trimBase } from "./api";
import { IMPORT_GRAPH_SPEC, specRelations } from "./spec";

export const queryClient = new QueryClient();

/** One row of a relation: its columns plus a stable key (the joined columns). */
export interface Row {
  id: string;
  cols: string[];
}

/** Runtime config the UI mutates; the sync reads it on each cycle. */
export const config = {
  api: DEFAULT_API,
  pollMs: 1500,
  paused: false,
};

// The relations the graph needs come from the view spec, so adding a relation to
// the spec wires up its sync automatically.
export const RELATIONS: string[] = specRelations(IMPORT_GRAPH_SPEC);
export type RelName = string;

/** Friendly column headers for known relations (the API only returns rows, not
 *  column names). Unknown relations fall back to positional headers. */
export const RELATION_COLUMNS: Record<string, string[]> = {
  module_node: ["module"],
  module_edge: ["from", "to"],
  workspace_node: ["workspace"],
  workspace_link: ["workspace", "module"],
  file_node: ["file", "module"],
  file_link: ["src", "dst"],
};

function relCollection(name: RelName) {
  return createCollection(
    queryCollectionOptions({
      queryClient,
      queryKey: [name],
      // A function so pausing / changing the interval takes effect on the next
      // cycle; `false` stops polling.
      refetchInterval: () => (config.paused ? false : config.pollMs),
      queryFn: async () => {
        const res = await fetch(`${trimBase(config.api)}/relations/${name}`);
        if (!res.ok) throw new Error(`${name}: ${res.status}`);
        const data = (await res.json()) as { rows?: string[][] };
        const rows = data.rows ?? [];
        return rows.map<Row>((cols) => ({ id: cols.join(""), cols }));
      },
      getKey: (item: Row) => item.id,
    }),
  );
}

export const collections: Record<string, ReturnType<typeof relCollection>> =
  Object.fromEntries(RELATIONS.map((name) => [name, relCollection(name)]));

/** Point the sync at a different engine and refetch immediately. */
export function setApi(api: string) {
  config.api = api;
  queryClient.invalidateQueries();
}

export function setPollMs(ms: number) {
  config.pollMs = Math.max(250, ms);
}

export function setPaused(paused: boolean) {
  config.paused = paused;
  if (!paused) queryClient.invalidateQueries();
}
