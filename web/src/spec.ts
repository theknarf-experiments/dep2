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
  /** Edge opacity 0..1 (default 1); muted edges read as secondary. */
  opacity?: number;
  /** Fixed edge color (default: the target node's color). */
  color?: string;
  /** Short human label for the HUD's edge-kind filter chips. */
  label?: string;
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

const WORKSPACE_COLOR = "#cfd2da";

/** Hardcoded spec for examples/import_graph.dl. */
export const IMPORT_GRAPH_SPEC: GraphSpec = {
  defaultView: "file",
  views: [
    { id: "crate", label: "Modules", nodes: ["module_node", "workspace_node"], edges: ["module_edge_live", "module_edge_pinned", "unused_dep", "workspace_link"] },
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
    file_link: { source: { ns: "f", col: 0 }, target: { ns: "f", col: 1 }, label: "imports" },
    // module_edge_live(from, to): workspace-linked dependency (changes flow
    // immediately: a `workspace:` spec, or an import with no version pin).
    module_edge_live: { source: { ns: "m", col: 0 }, target: { ns: "m", col: 1 }, label: "live deps" },
    // module_edge_pinned(from, to): dependency pinned to a PUBLISHED version
    // of a workspace module — muted: local changes only flow on publish+bump.
    module_edge_pinned: { source: { ns: "m", col: 0 }, target: { ns: "m", col: 1 }, opacity: 0.3, label: "pinned deps" },
    // unused_dep(module, dep): declared but never imported — the warning
    // overlay paints dependency cruft directly onto the module graph.
    unused_dep: { source: { ns: "m", col: 0 }, target: { ns: "m", col: 1 }, color: "#e5484d", opacity: 0.85, label: "unused deps" },
    // workspace_link(workspace, module): workspace membership.
    workspace_link: { source: { ns: "w", col: 0 }, target: { ns: "m", col: 1 }, label: "workspace" },
  },
  nodeClasses: [
    // test_file(file): test/spec/__tests__/__mocks__ files, hideable to
    // declutter the Files view.
    { rel: "test_file", ns: "f", col: 0, label: "tests" },
    // Cycle members: solo them to spotlight dependency cycles.
    { rel: "cycle_file", ns: "f", col: 0, label: "cycles" },
    { rel: "cycle_module", ns: "m", col: 0, label: "cycles" },
  ],
  sizes: {
    lg: { radius: 14, alwaysLabel: true, fontSize: 8 },
    md: { radius: 9, alwaysLabel: true, fontSize: 6 },
    sm: { radius: 4, alwaysLabel: false, fontSize: 4.5 },
  },
};

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
