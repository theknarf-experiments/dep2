// Generic graph data the renderer understands. Consumers map their own domain
// model onto these (e.g. dep2 maps relations -> nodes/edges in web/src/model.ts).

export interface GraphNode {
  /** Stable identity; edges reference nodes by this. */
  id: string;
  /** Text drawn as the node's label. */
  label: string;
  /** Any CSS color string (hex, rgb, hsl). */
  color: string;
  /** Optional grouping; pass the same value as `activeGroup` to spotlight it. */
  group?: string;
  /** Node radius in world units (default {@link DEFAULT_RADIUS}). */
  radius?: number;
  /** Show the label even when the node isn't hovered/selected (default false). */
  alwaysLabel?: boolean;
  /** Label font size in world units (default {@link DEFAULT_FONT_SIZE}). */
  fontSize?: number;
}

export interface GraphEdge {
  id: string;
  source: string; // a GraphNode id
  target: string; // a GraphNode id
}

export interface GraphElements {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

/** Frame-timing readout the renderer writes each ~0.5s; surface it in a HUD. */
export interface Perf {
  fps: number;
  worstMs: number;
}

export const DEFAULT_RADIUS = 5;
export const DEFAULT_FONT_SIZE = 6;
