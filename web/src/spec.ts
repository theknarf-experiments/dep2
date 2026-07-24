// Graph view spec: a *declarative* description of how relation rows become graph
// nodes and edges. The renderer (model.ts) is a generic interpreter of this — it
// has no knowledge of any specific analysis. For now the spec is hardcoded for
// the import-graph program; later it can come from the engine (an endpoint, a
// `.dl` directive, or a convention over `/relations`) with no code changes here.

export type ColIndex = number;

/** Visual preset for a class of nodes. */
export interface SizePreset {
  radius: number;
  alwaysLabel: boolean;
  fontSize: number;
}

/** How one relation's rows become nodes. */
export interface NodeSpec {
  /** Id namespace, so ids from different relations never collide and edges can
   *  resolve an endpoint to the right relation (`${ns}:${idValue}`). */
  ns: string;
  /** Column holding the node id. */
  id: ColIndex;
  /** Column holding the label text. */
  label: ColIndex;
  /** Optional transform applied to the label value. */
  labelTransform?: "basename";
  /** Column holding the full title (info panel). */
  title: ColIndex;
  /** Column holding the group/cluster key (legend + default color). */
  group: ColIndex;
  /** Fixed color; when omitted the group value is colored via `colorFor`. */
  color?: string;
  /** Visual preset key (see `GraphSpec.sizes`). */
  size: string;
  /** Display category, shown in the info panel. */
  kind: string;
  /** Column holding a repo-relative file path: the info panel offers
   *  "open in Code" for such nodes. */
  sourcePath?: ColIndex;
}

/** One endpoint of an edge: which node namespace it lives in, and the column. */
export interface EdgeEndpoint {
  ns: string;
  col: ColIndex;
}

/** How one relation's rows become edges. */
export interface EdgeSpec {
  source: EdgeEndpoint;
  target: EdgeEndpoint;
  /** Edge opacity 0..1 (default 1); muted edges read as secondary. */
  opacity?: number;
  /** Fixed edge color (default: the target node's color). */
  color?: string;
  /** Short human label for the HUD's edge-kind filter chips. */
  label?: string;
  /** Columns locating the edge's SOURCE SITE in the repo (e.g. a call
   *  site): the info panel links each neighbor to it in the Code view. */
  at?: { file: ColIndex; line?: ColIndex };
  /** Synthesize nodes for edge endpoints with no node relation (call graphs
   *  etc.): endpoint values become nodes labeled by the value, optionally
   *  grouped by another edge column. */
  derive?: {
    size?: string;
    kind?: string;
    sourceGroup?: ColIndex;
    targetGroup?: ColIndex;
  };
}

/** A named view: the subset of node/edge relations it shows. */
export interface ViewSpec {
  id: string;
  label: string;
  nodes: string[];
  edges: string[];
}

/** A relation whose rows mark a CLASS of nodes (by id), toggleable in the
 * HUD filter bar — e.g. test files. */
export interface NodeClassSpec {
  rel: string;
  ns: string;
  col: ColIndex;
  label: string;
}

/** The full spec: views + per-relation node/edge mappings + size presets. */
export interface GraphSpec {
  defaultView: string;
  views: ViewSpec[];
  nodes: Record<string, NodeSpec>;
  edges: Record<string, EdgeSpec>;
  /** Optional toggleable node classes (chips appear when non-empty). */
  nodeClasses?: NodeClassSpec[];
  sizes: Record<string, SizePreset>;
}

/** Every relation the spec references (nodes + edges), de-duplicated. */
export function specRelations(spec: GraphSpec): string[] {
  return [
    ...new Set([
      ...Object.keys(spec.nodes),
      ...Object.keys(spec.edges),
      ...(spec.nodeClasses ?? []).map((c) => c.rel),
    ]),
  ];
}

/** Resolve a view by id, falling back to the default (then the first). */
export function resolveView(spec: GraphSpec, id: string): ViewSpec {
  return (
    spec.views.find((v) => v.id === id) ??
    spec.views.find((v) => v.id === spec.defaultView) ??
    spec.views[0]
  );
}
