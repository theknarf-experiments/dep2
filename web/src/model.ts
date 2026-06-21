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
  file_edge: string[][];
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

  // File view: every file points to the workspace crates it imports. Files are
  // colored by their owning crate; crate nodes are larger anchors.
  const fileCrate = new Map(rels.file_node.map(([f, c]) => [f, c]));
  const nodes: GNode[] = [];
  for (const [c] of rels.crate_node) {
    nodes.push({
      id: `c:${c}`,
      label: c,
      title: c,
      group: c,
      kind: "crate",
      color: colorFor(c),
    });
  }
  for (const [f] of rels.file_node) {
    const c = fileCrate.get(f) ?? "";
    nodes.push({
      id: `f:${f}`,
      label: basename(f),
      title: f,
      group: c,
      kind: "file",
      color: colorFor(c),
    });
  }
  const edges: GEdge[] = [];
  // Rust: file -> the external workspace crate it imports.
  for (const [f, to] of rels.file_edge) {
    edges.push({ id: `e:${f}->c:${to}`, source: `f:${f}`, target: `c:${to}` });
  }
  // Intra-project: file -> file (Rust module tree / crate::/super:: use, JS imports).
  for (const [src, dst] of rels.file_link) {
    edges.push({ id: `e:${src}->f:${dst}`, source: `f:${src}`, target: `f:${dst}` });
  }
  return { nodes, edges };
}
