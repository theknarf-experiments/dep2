// Turn relation rows into graph elements for each view mode.

// "crate" is the module view (modules + the workspace); "file" is per-file.
export type Mode = "crate" | "file";
export type Kind = "module" | "file" | "workspace";

export interface GNode {
  id: string;
  label: string;
  title: string;
  group: string; // module the node belongs to (drives color)
  kind: Kind;
  color: string;
}

export interface GEdge {
  id: string;
  source: string;
  target: string;
}

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

/** Stable, well-spread color per group name (deterministic hash -> HSL). */
export function colorFor(name: string): string {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) | 0;
  const hue = ((h % 360) + 360) % 360;
  return `hsl(${hue}, 65%, 58%)`;
}

const basename = (p: string): string => p.split("/").pop() ?? p;

export interface RawRelations {
  module_node: string[][];
  module_edge: string[][];
  workspace_node: string[][];
  workspace_link: string[][];
  file_node: string[][];
  file_link: string[][];
}

export function buildElements(mode: Mode, rels: RawRelations): GraphElements {
  if (mode === "crate") {
    // Module view: one node per module + the workspace, edges are cross-module
    // dependencies plus workspace membership.
    const nodes: GNode[] = rels.module_node.map(([m]) => ({
      id: `m:${m}`,
      label: m,
      title: m,
      group: m,
      kind: "module",
      color: colorFor(m),
    }));
    for (const [w] of rels.workspace_node) {
      nodes.push({ id: `w:${w}`, label: w, title: w, group: w, kind: "workspace", color: WORKSPACE_COLOR });
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
  const nodes: GNode[] = rels.file_node.map(([f, m]) => ({
    id: `f:${f}`,
    label: basename(f),
    title: f,
    group: m,
    kind: "file",
    color: colorFor(m),
  }));
  const edges: GEdge[] = rels.file_link.map(([src, dst]) => ({
    id: `e:${src}->${dst}`,
    source: `f:${src}`,
    target: `f:${dst}`,
  }));
  return { nodes, edges };
}
