// Bridge TanStack DB -> graph elements. Snapshots the spec's relations from the
// reactive store and re-renders as the poll syncs new rows; the relation -> graph
// derivation lives in model.ts, driven by the spec (spec.ts). Reading the spec's
// relations in a loop (rather than a fixed set of live-query hooks) keeps this
// agnostic to which relations the spec references.

import { useEffect, useMemo, useState } from "react";
import { collections } from "./db";
import { buildElements, GraphElements, Mode, RawRows } from "./model";
import { IMPORT_GRAPH_SPEC, specRelations } from "./spec";

const SPEC = IMPORT_GRAPH_SPEC;
const RELS = specRelations(SPEC);

/** Live snapshot of every spec relation's rows, refreshed on any change. */
function useSpecRows(): RawRows {
  const [raw, setRaw] = useState<RawRows>({});
  useEffect(() => {
    const read = (): RawRows => {
      const out: RawRows = {};
      for (const r of RELS) {
        const c = collections[r];
        out[r] = c ? c.toArray.map((row) => row.cols) : [];
      }
      return out;
    };
    setRaw(read());
    // Subscribing also activates each collection's polling sync; preload nudges
    // it to start immediately rather than on the next interval.
    const subs = RELS.map((r) => {
      const c = collections[r];
      void c?.preload();
      return c?.subscribeChanges(() => setRaw(read()));
    });
    return () => subs.forEach((sub) => sub?.unsubscribe());
  }, []);
  return raw;
}

export function useGraphData(mode: Mode): { elements: GraphElements; loading: boolean } {
  const raw = useSpecRows();
  const elements = useMemo(() => buildElements(SPEC, mode, raw), [mode, raw]);
  const loading = !RELS.some((r) => (raw[r]?.length ?? 0) > 0);
  return { elements, loading };
}
