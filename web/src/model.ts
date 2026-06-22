// Turn relation rows into graph elements for each view mode. The rendering types
// come from the reusable @dep2/force-graph package; dep2 adds `kind`/`title` for
// its info panel and maps each kind to a visual size/label preset.

import { colorFor, GraphEdge, GraphNode } from "@dep2/force-graph";

export { colorFor };

// "crate" is the module view (modules + the workspace); "file" is per-file.
export type Mode = "crate" | "file";
export type Kind = "module" | "file" | "workspace";

export interface GNode extends GraphNode {
  group: string; // always set here (overrides the optional in GraphNode)
  title: string;
  kind: Kind;
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
  kind: Kind;
  imports: string[];
  importedBy: string[];
}

const WORKSPACE_COLOR = "#cfd2da";

// Per-kind visuals: bigger/always-labelled for modules and the workspace, small
// and label-on-hover for files.
const VIS: Record<Kind, { radius: number; alwaysLabel: boolean; fontSize: number }> = {
  workspace: { radius: 14, alwaysLabel: true, fontSize: 8 },
  module: { radius: 9, alwaysLabel: true, fontSize: 6 },
  file: { radius: 4, alwaysLabel: false, fontSize: 4.5 },
};

const basename = (p: string): string => p.split("/").pop() ?? p;

export interface RawRelations {
  module_node: string[][];
  module_edge: string[][];
  workspace_node: string[][];
  workspace_link: string[][];
  file_node: string[][];
  file_link: string[][];
}

function node(id: string, label: string, title: string, group: string, kind: Kind, color: string): GNode {
  return { id, label, title, group, kind, color, ...VIS[kind] };
}

export function buildElements(mode: Mode, rels: RawRelations): GraphElements {
  if (mode === "crate") {
    // Module view: one node per module + the workspace, edges are cross-module
    // dependencies plus workspace membership.
    const nodes: GNode[] = rels.module_node.map(([m]) => node(`m:${m}`, m, m, m, "module", colorFor(m)));
    for (const [w] of rels.workspace_node) {
      nodes.push(node(`w:${w}`, w, w, w, "workspace", WORKSPACE_COLOR));
    }
    const edges: GEdge[] = [];
    for (const [from, to] of rels.module_edge) {
      edges.push({ id: `e:${from}->${to}`, source: `m:${from}`, target: `m:${to}` });
    }
    for (const [w, m] of rels.workspace_link) {
      edges.push({ id: `wl:${w}->${m}`, source: `w:${w}`, target: `m:${m}` });
    }
    return { nodes, edges };
  }

  // File view: one node per source file, colored by its module; edges are the
  // intra-module file -> file dependencies.
  const nodes: GNode[] = rels.file_node.map(([f, m]) => node(`f:${f}`, basename(f), f, m, "file", colorFor(m)));
  const edges: GEdge[] = rels.file_link.map(([src, dst]) => ({
    id: `e:${src}->${dst}`,
    source: `f:${src}`,
    target: `f:${dst}`,
  }));
  return { nodes, edges };
}
