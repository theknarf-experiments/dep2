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

export interface GEdge extends GraphEdge {
  /** The relation this edge came from (drives the HUD edge-kind filters). */
  rel: string;
}

export interface GraphElements {
  nodes: GNode[];
  edges: GEdge[];
}

/** A neighbor entry in the info panel: enough to display it and to point back
 * at its node in the graph (hover-highlight, click-to-focus). */
export interface NodeRef {
  id: string;
  title: string;
}

/** Details for the clicked node, shown in the HUD info panel. */
export interface SelectedInfo {
  id: string;
  label: string;
  title: string;
  group: string;
  kind: string;
  imports: NodeRef[];
  importedBy: NodeRef[];
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
        rel,
        source: `${es.source.ns}:${s}`,
        target: `${es.target.ns}:${t}`,
        ...(es.opacity !== undefined ? { opacity: es.opacity } : {}),
      });
    }
  }

  return { nodes, edges };
}

/** Live client-side filters over the built elements. */
export interface Filters {
  /** Case-insensitive substring over node titles; matches keep 1-hop context. */
  query: string;
  /** Edge relations currently hidden. */
  hiddenRels: Set<string>;
  /** Drop nodes with no visible edges (after the other filters). */
  hideIsolated: boolean;
}

export const EMPTY_FILTERS: Filters = {
  query: "",
  hiddenRels: new Set(),
  hideIsolated: false,
};

/** Apply [`Filters`] to built elements: edge-kind toggles first, then the
 * text query (matching nodes keep their direct neighbors for context), then
 * the isolated-node drop. Pure, cheap (runs per keystroke over the already
 * fetched rows), and stable — node identity survives so the layout keeps
 * positions. */
export function applyFilters(elements: GraphElements, f: Filters): GraphElements {
  let { nodes, edges } = elements;
  if (f.hiddenRels.size > 0) {
    edges = edges.filter((e) => !f.hiddenRels.has(e.rel));
  }
  const q = f.query.trim().toLowerCase();
  if (q) {
    const matched = new Set(
      nodes.filter((n) => n.title.toLowerCase().includes(q) || n.label.toLowerCase().includes(q)).map((n) => n.id),
    );
    edges = edges.filter((e) => matched.has(e.source) || matched.has(e.target));
    const keep = new Set(matched);
    for (const e of edges) {
      keep.add(e.source);
      keep.add(e.target);
    }
    nodes = nodes.filter((n) => keep.has(n.id));
  }
  if (f.hideIsolated) {
    const touched = new Set<string>();
    for (const e of edges) {
      touched.add(e.source);
      touched.add(e.target);
    }
    nodes = nodes.filter((n) => touched.has(n.id));
    // Edges always reference surviving nodes here (touched ⊇ endpoints).
  }
  if (nodes !== elements.nodes || edges !== elements.edges) {
    return { nodes, edges };
  }
  return elements;
}
