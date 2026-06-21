// Bridge TanStack DB -> graph elements. Each relation is read with a live query
// so this re-renders incrementally as the poll syncs new rows; the per-mode
// node/edge derivation lives in model.ts.

import { useMemo } from "react";
import { useLiveQuery } from "@tanstack/react-db";
import { collections, Row } from "./db";
import { buildElements, GraphElements, Mode, RawRelations } from "./model";

const rows = (data: readonly Row[] | undefined): string[][] =>
  (data ?? []).map((r) => r.cols);

export function useGraphData(mode: Mode): { elements: GraphElements; loading: boolean } {
  const crateNode = useLiveQuery((q) => q.from({ r: collections.crate_node }));
  const crateEdge = useLiveQuery((q) => q.from({ r: collections.crate_edge }));
  const fileNode = useLiveQuery((q) => q.from({ r: collections.file_node }));
  const fileEdge = useLiveQuery((q) => q.from({ r: collections.file_edge }));
  const fileLink = useLiveQuery((q) => q.from({ r: collections.file_link }));

  const raw: RawRelations = useMemo(
    () => ({
      crate_node: rows(crateNode.data),
      crate_edge: rows(crateEdge.data),
      file_node: rows(fileNode.data),
      file_edge: rows(fileEdge.data),
      file_link: rows(fileLink.data),
    }),
    [crateNode.data, crateEdge.data, fileNode.data, fileEdge.data, fileLink.data],
  );

  const elements = useMemo(() => buildElements(mode, raw), [mode, raw]);
  const loading = crateNode.isLoading || fileNode.isLoading;
  return { elements, loading };
}
