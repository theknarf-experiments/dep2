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
  const moduleNode = useLiveQuery((q) => q.from({ r: collections.module_node }));
  const moduleEdge = useLiveQuery((q) => q.from({ r: collections.module_edge }));
  const workspaceNode = useLiveQuery((q) => q.from({ r: collections.workspace_node }));
  const workspaceLink = useLiveQuery((q) => q.from({ r: collections.workspace_link }));
  const fileNode = useLiveQuery((q) => q.from({ r: collections.file_node }));
  const fileLink = useLiveQuery((q) => q.from({ r: collections.file_link }));

  const raw: RawRelations = useMemo(
    () => ({
      module_node: rows(moduleNode.data),
      module_edge: rows(moduleEdge.data),
      workspace_node: rows(workspaceNode.data),
      workspace_link: rows(workspaceLink.data),
      file_node: rows(fileNode.data),
      file_link: rows(fileLink.data),
    }),
    [
      moduleNode.data,
      moduleEdge.data,
      workspaceNode.data,
      workspaceLink.data,
      fileNode.data,
      fileLink.data,
    ],
  );

  const elements = useMemo(() => buildElements(mode, raw), [mode, raw]);
  const loading = fileNode.isLoading || moduleNode.isLoading;
  return { elements, loading };
}
