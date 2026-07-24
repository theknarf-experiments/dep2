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
        ...(es.color !== undefined ? { color: es.color } : {}),
      });
    }
  }

  return { nodes, edges };
}

/** Live client-side filters over the built elements. */
export interface Filters {
  /** Hard-scope to one group/module: nodes outside it are HIDDEN (legend
   * click toggles; hover still spotlights). */
  scopeGroup: string | null;
  /** Case-insensitive substring over node titles; matches keep 1-hop context. */
  query: string;
  /** Edge relations currently hidden. */
  hiddenRels: Set<string>;
  /** Node-class relation states: solo ("only") or hidden. Absent = neutral. */
  classStates: Map<string, "only" | "hidden">;
  /** Isolated-node handling: hide them, show ONLY them, or neither. */
  isolated: "hide" | "only" | null;
  /** Hide edges whose endpoints belong to different groups (modules). */
  hideCrossModule: boolean;
}

export const EMPTY_FILTERS: Filters = {
  scopeGroup: null,
  query: "",
  hiddenRels: new Set(),
  classStates: new Map(),
  isolated: null,
  hideCrossModule: false,
};

/** Node-id sets per class relation (see `GraphSpec.nodeClasses`). */
export type NodeClasses = Map<string, Set<string>>;

/** Build the class-relation id sets from raw rows. */
export function buildClasses(spec: GraphSpec, raw: RawRows): NodeClasses {
  const out: NodeClasses = new Map();
  for (const c of spec.nodeClasses ?? []) {
    const ids = new Set<string>();
    for (const cols of raw[c.rel] ?? []) {
      const v = cols[c.col];
      if (v !== undefined) ids.add(`${c.ns}:${v}`);
    }
    out.set(c.rel, ids);
  }
  return out;
}

/** Apply [`Filters`] to built elements: edge-kind toggles first, then the
 * text query (matching nodes keep their direct neighbors for context), then
 * the isolated-node drop. Pure, cheap (runs per keystroke over the already
 * fetched rows), and stable — node identity survives so the layout keeps
 * positions. */
export function applyFilters(
  elements: GraphElements,
  f: Filters,
  classes: NodeClasses = new Map(),
): GraphElements {
  let { nodes, edges } = elements;
  if (f.scopeGroup) {
    nodes = nodes.filter((n) => n.group === f.scopeGroup);
    const keep = new Set(nodes.map((n) => n.id));
    edges = edges.filter((e) => keep.has(e.source) && keep.has(e.target));
  }
  if (f.hiddenRels.size > 0) {
    edges = edges.filter((e) => !f.hiddenRels.has(e.rel));
  }
  if (f.hideCrossModule) {
    const groupOf = new Map(nodes.map((n) => [n.id, n.group]));
    edges = edges.filter((e) => groupOf.get(e.source) === groupOf.get(e.target));
  }
  {
    // Solo classes first (keep only their union), then hidden classes.
    const solo = new Set<string>();
    let anySolo = false;
    const hidden = new Set<string>();
    for (const [rel, state] of f.classStates) {
      const ids = classes.get(rel) ?? new Set();
      if (state === "only") {
        anySolo = true;
        for (const id of ids) solo.add(id);
      } else {
        for (const id of ids) hidden.add(id);
      }
    }
    if (anySolo) {
      nodes = nodes.filter((n) => solo.has(n.id));
      const keep = new Set(nodes.map((n) => n.id));
      edges = edges.filter((e) => keep.has(e.source) && keep.has(e.target));
    }
    if (hidden.size > 0) {
      nodes = nodes.filter((n) => !hidden.has(n.id));
      edges = edges.filter((e) => !hidden.has(e.source) && !hidden.has(e.target));
    }
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
  if (f.isolated) {
    const touched = new Set<string>();
    for (const e of edges) {
      touched.add(e.source);
      touched.add(e.target);
    }
    if (f.isolated === "hide") {
      nodes = nodes.filter((n) => touched.has(n.id));
      // Edges always reference surviving nodes (touched ⊇ endpoints).
    } else {
      // Orphans only: nodes with no visible edges; no edges survive.
      nodes = nodes.filter((n) => !touched.has(n.id));
      edges = [];
    }
  }
  if (nodes !== elements.nodes || edges !== elements.edges) {
    return { nodes, edges };
  }
  return elements;
}
