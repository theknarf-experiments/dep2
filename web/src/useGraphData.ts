// Bridge TanStack DB -> graph elements. Snapshots the spec's relations from the
// reactive store and re-renders as the poll syncs new rows; the relation -> graph
// derivation lives in model.ts, driven by the FETCHED spec (GET /spec), so this
// file is agnostic to which analysis is loaded.

import { useEffect, useMemo, useState } from "react";
import { collectionFor } from "./db";
import {
  buildClasses,
  buildElements,
  GraphElements,
  Mode,
  NodeClasses,
  RawRows,
} from "./model";
import { GraphSpec, specRelations } from "./spec";

/** Live snapshot of every spec relation's rows, refreshed on any change. */
function useSpecRows(spec: GraphSpec | null): RawRows {
  const rels = useMemo(() => (spec ? specRelations(spec) : []), [spec]);
  const [raw, setRaw] = useState<RawRows>({});
  useEffect(() => {
    if (rels.length === 0) return;
    const read = (): RawRows => {
      const out: RawRows = {};
      for (const r of rels) {
        out[r] = collectionFor(r).toArray.map((row) => row.cols);
      }
      return out;
    };
    setRaw(read());
    // Subscribing also activates each collection's polling sync; preload nudges
    // it to start immediately rather than on the next interval.
    const subs = rels.map((r) => {
      const c = collectionFor(r);
      void c.preload();
      return c.subscribeChanges(() => setRaw(read()));
    });
    return () => subs.forEach((sub) => sub.unsubscribe());
  }, [rels]);
  return raw;
}

export function useGraphData(
  spec: GraphSpec | null,
  mode: Mode,
): {
  elements: GraphElements;
  classes: NodeClasses;
  loading: boolean;
} {
  const raw = useSpecRows(spec);
  const elements = useMemo(
    () => (spec ? buildElements(spec, mode, raw) : { nodes: [], edges: [] }),
    [spec, mode, raw],
  );
  const classes = useMemo(
    () => (spec ? buildClasses(spec, raw) : new Map()),
    [spec, raw],
  );
  const loading =
    !spec || !specRelations(spec).some((r) => (raw[r]?.length ?? 0) > 0);
  return { elements, classes, loading };
}
