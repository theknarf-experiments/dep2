// Turn relation rows into cytoscape-ready elements for each view mode.

export type Mode = "crate" | "file";

export interface GNode {
  id: string;
  label: string;
  title: string;
  group: string; // crate the node belongs to (drives color)
  kind: "crate" | "file";
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
  kind: "crate" | "file";
  imports: string[];
  importedBy: string[];
}

/** Stable, well-spread color per group name (deterministic hash -> HSL). */
export function colorFor(name: string): string {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) | 0;
  const hue = ((h % 360) + 360) % 360;
  return `hsl(${hue}, 65%, 58%)`;
}

const basename = (p: string): string => p.split("/").pop() ?? p;

export interface RawRelations {
  crate_node: string[][];
  crate_edge: string[][];
  file_node: string[][];
  file_link: string[][];
}

export function buildElements(mode: Mode, rels: RawRelations): GraphElements {
  if (mode === "crate") {
    const nodes: GNode[] = rels.crate_node.map(([c]) => ({
      id: `c:${c}`,
      label: c,
      title: c,
      group: c,
      kind: "crate",
      color: colorFor(c),
    }));
    const edges: GEdge[] = rels.crate_edge.map(([from, to]) => ({
      id: `e:${from}->${to}`,
      source: `c:${from}`,
      target: `c:${to}`,
    }));
    return { nodes, edges };
  }

  // File view: one node per source file, colored by its crate/dir. Every edge is
  // a file -> file dependency (file_link already resolves cross-crate imports to
  // the imported crate's lib.rs), so there are no standalone crate anchors.
  const nodes: GNode[] = rels.file_node.map(([f, c]) => ({
    id: `f:${f}`,
    label: basename(f),
    title: f,
    group: c,
    kind: "file",
    color: colorFor(c),
  }));
  const edges: GEdge[] = rels.file_link.map(([src, dst]) => ({
    id: `e:${src}->${dst}`,
    source: `f:${src}`,
    target: `f:${dst}`,
  }));
  return { nodes, edges };
}
