// Shared frame-timing readout: the renderer writes it, the HUD displays it.
export interface Perf {
  fps: number;
  worstMs: number;
}
