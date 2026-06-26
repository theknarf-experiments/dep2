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
}

/** A named view: the subset of node/edge relations it shows. */
export interface ViewSpec {
  id: string;
  label: string;
  nodes: string[];
  edges: string[];
}

/** The full spec: views + per-relation node/edge mappings + size presets. */
export interface GraphSpec {
  defaultView: string;
  views: ViewSpec[];
  nodes: Record<string, NodeSpec>;
  edges: Record<string, EdgeSpec>;
  sizes: Record<string, SizePreset>;
}

const WORKSPACE_COLOR = "#cfd2da";

/** Hardcoded spec for examples/import_graph.dl. */
export const IMPORT_GRAPH_SPEC: GraphSpec = {
  defaultView: "file",
  views: [
    { id: "crate", label: "Modules", nodes: ["module_node", "workspace_node"], edges: ["module_edge", "workspace_link"] },
    { id: "file", label: "Files", nodes: ["file_node"], edges: ["file_link"] },
  ],
  nodes: {
    // file_node(file, module): one node per source file, colored by its module.
    file_node: { ns: "f", id: 0, label: 0, labelTransform: "basename", title: 0, group: 1, size: "sm", kind: "file" },
    // module_node(module): one node per module.
    module_node: { ns: "m", id: 0, label: 0, title: 0, group: 0, size: "md", kind: "module" },
    // workspace_node(workspace): the workspace root, fixed neutral color.
    workspace_node: { ns: "w", id: 0, label: 0, title: 0, group: 0, color: WORKSPACE_COLOR, size: "lg", kind: "workspace" },
  },
  edges: {
    // file_link(src, dst): intra-module file -> file dependency.
    file_link: { source: { ns: "f", col: 0 }, target: { ns: "f", col: 1 } },
    // module_edge(from, to): cross-module dependency.
    module_edge: { source: { ns: "m", col: 0 }, target: { ns: "m", col: 1 } },
    // workspace_link(workspace, module): workspace membership.
    workspace_link: { source: { ns: "w", col: 0 }, target: { ns: "m", col: 1 } },
  },
  sizes: {
    lg: { radius: 14, alwaysLabel: true, fontSize: 8 },
    md: { radius: 9, alwaysLabel: true, fontSize: 6 },
    sm: { radius: 4, alwaysLabel: false, fontSize: 4.5 },
  },
};

/** Every relation the spec references (nodes + edges), de-duplicated. */
export function specRelations(spec: GraphSpec): string[] {
  return [...new Set([...Object.keys(spec.nodes), ...Object.keys(spec.edges)])];
}

/** Resolve a view by id, falling back to the default (then the first). */
export function resolveView(spec: GraphSpec, id: string): ViewSpec {
  return (
    spec.views.find((v) => v.id === id) ??
    spec.views.find((v) => v.id === spec.defaultView) ??
    spec.views[0]
  );
}
