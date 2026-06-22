// Self-contained variant: <ForceGraph> already lives inside an R3F <Canvas>, so
// consumers that don't manage their own Canvas (or Storybook) can use this.

import { CSSProperties } from "react";
import { Canvas } from "@react-three/fiber";
import { ForceGraph, ForceGraphProps } from "./ForceGraph";

export interface ForceGraphCanvasProps extends ForceGraphProps {
  /** Canvas clear color (default #0e0e11). */
  background?: string;
  style?: CSSProperties;
  className?: string;
}

export function ForceGraphCanvas({
  background = "#0e0e11",
  style,
  className,
  ...graph
}: ForceGraphCanvasProps) {
  return (
    <Canvas
      className={className}
      style={{ position: "absolute", inset: 0, ...style }}
      gl={{ antialias: true }}
      flat
      dpr={[1, 2]}
    >
      <color attach="background" args={[background]} />
      <ForceGraph {...graph} />
    </Canvas>
  );
}
