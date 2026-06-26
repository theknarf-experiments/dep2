// Generic interpreter: turn relation rows into graph elements per a `GraphSpec`
// (see spec.ts). This file knows nothing about any specific analysis — all the
// "which relation is a node/edge and which columns mean what" lives in the spec,
// so a different analysis only needs a different spec.

import { colorFor, GraphEdge, GraphNode } from "@dep2/force-graph";
import { GraphSpec, resolveView } from "./spec";

export { colorFor };

/** A view id (e.g. "file", "crate") — one of the spec's `views`. */
export type Mode = string;

export interface GNode extends GraphNode {
  group: string; // always set here (overrides the optional in GraphNode)
  title: string;
  kind: string;
}

export type GEdge = GraphEdge;

export interface GraphElements {
  nodes: GNode[];
  edges: GEdge[];
}

/** Details for the clicked node, shown in the HUD info panel. */
export interface SelectedInfo {
  id: string;
  label: string;
  title: string;
  group: string;
  kind: string;
  imports: string[];
  importedBy: string[];
}

/** Rows keyed by relation name, as fetched from the query API. */
export type RawRows = Record<string, string[][]>;

const basename = (p: string): string => p.split("/").pop() ?? p;

function transform(value: string, t: "basename" | undefined): string {
  return t === "basename" ? basename(value) : value;
}

/** Build the graph for `viewId` by interpreting `spec` over `raw` relation rows. */
export function buildElements(spec: GraphSpec, viewId: Mode, raw: RawRows): GraphElements {
  const view = resolveView(spec, viewId);

  const nodes: GNode[] = [];
  for (const rel of view.nodes) {
    const ns = spec.nodes[rel];
    if (!ns) continue;
    const preset = spec.sizes[ns.size];
    for (const cols of raw[rel] ?? []) {
      const idVal = cols[ns.id] ?? "";
      const group = cols[ns.group] ?? "";
      const label = transform(cols[ns.label] ?? "", ns.labelTransform);
      nodes.push({
        id: `${ns.ns}:${idVal}`,
        label,
        title: cols[ns.title] ?? idVal,
        group,
        kind: ns.kind,
        color: ns.color ?? colorFor(group),
        ...preset,
      });
    }
  }

  const edges: GEdge[] = [];
  for (const rel of view.edges) {
    const es = spec.edges[rel];
    if (!es) continue;
    for (const cols of raw[rel] ?? []) {
      const s = cols[es.source.col] ?? "";
      const t = cols[es.target.col] ?? "";
      edges.push({
        id: `${rel}:${s}->${t}`,
        source: `${es.source.ns}:${s}`,
        target: `${es.target.ns}:${t}`,
      });
    }
  }

  return { nodes, edges };
}
