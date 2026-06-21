import { useEffect, useRef } from "react";
import cytoscape, { Core, ElementDefinition, LayoutOptions } from "cytoscape";
import fcose from "cytoscape-fcose";
import { GraphElements, Mode } from "./model";

cytoscape.use(fcose);

const STYLE: cytoscape.StylesheetStyle[] = [
  {
    selector: "node",
    style: {
      "background-color": "data(color)",
      label: "data(label)",
      color: "#e8e8ea",
      "font-size": 10,
      "text-valign": "center",
      "text-halign": "center",
      "text-outline-color": "#16161a",
      "text-outline-width": 2,
      "min-zoomed-font-size": 6,
    },
  },
  {
    selector: 'node[kind="crate"]',
    style: {
      shape: "round-rectangle",
      width: "label",
      height: 26,
      padding: "8px",
      "font-size": 12,
      "font-weight": 700,
      "border-width": 2,
      "border-color": "#16161a",
    },
  },
  {
    selector: 'node[kind="file"]',
    style: { shape: "ellipse", width: 12, height: 12, "font-size": 8 },
  },
  {
    selector: "edge",
    style: {
      width: 1.5,
      "line-color": "#4a4a55",
      "target-arrow-color": "#4a4a55",
      "target-arrow-shape": "triangle",
      "arrow-scale": 0.9,
      "curve-style": "bezier",
      opacity: 0.7,
    },
  },
  {
    selector: ".faded",
    style: { opacity: 0.12, "text-opacity": 0.12 },
  },
  {
    selector: ".hi",
    style: { "line-color": "#ffd166", "target-arrow-color": "#ffd166", width: 2.5, opacity: 1 },
  },
  {
    selector: "node.hi",
    style: { "border-width": 3, "border-color": "#ffd166", opacity: 1, "text-opacity": 1 },
  },
];

function layoutOpts(initial: boolean): LayoutOptions {
  return {
    name: "fcose",
    // Preserve existing node positions on live updates so the graph doesn't
    // jump every poll; only do a fresh randomized layout on first paint / mode
    // switch.
    randomize: initial,
    animate: !initial,
    animationDuration: 350,
    fit: initial,
    padding: 40,
    nodeSeparation: 90,
    idealEdgeLength: 110,
    nodeRepulsion: 9000,
  } as unknown as LayoutOptions;
}

function toDefs(els: GraphElements): ElementDefinition[] {
  const defs: ElementDefinition[] = [];
  for (const n of els.nodes) {
    defs.push({ data: { id: n.id, label: n.label, title: n.title, color: n.color, kind: n.kind } });
  }
  for (const e of els.edges) {
    defs.push({ data: { id: e.id, source: e.source, target: e.target } });
  }
  return defs;
}

interface Props {
  elements: GraphElements;
  mode: Mode;
}

export function Graph({ elements, mode }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const cyRef = useRef<Core | null>(null);
  const prevMode = useRef<Mode>(mode);

  // Create the cytoscape instance once.
  useEffect(() => {
    if (!containerRef.current) return;
    const cy = cytoscape({
      container: containerRef.current,
      style: STYLE,
      minZoom: 0.1,
      maxZoom: 3,
      wheelSensitivity: 0.2,
    });
    cyRef.current = cy;

    // Hover/select: highlight a node and its direct neighborhood.
    cy.on("tap", "node", (evt) => {
      const node = evt.target;
      cy.elements().addClass("faded").removeClass("hi");
      const hood = node.closedNeighborhood();
      hood.removeClass("faded").addClass("hi");
    });
    cy.on("tap", (evt) => {
      if (evt.target === cy) cy.elements().removeClass("faded hi");
    });

    return () => {
      cy.destroy();
      cyRef.current = null;
    };
  }, []);

  // Reconcile elements on every data/mode change.
  useEffect(() => {
    const cy = cyRef.current;
    if (!cy) return;
    const defs = toDefs(elements);
    const modeChanged = prevMode.current !== mode;
    prevMode.current = mode;

    cy.batch(() => {
      if (modeChanged) {
        cy.elements().remove();
        cy.add(defs);
        return;
      }
      const wanted = new Set(defs.map((d) => d.data.id as string));
      // Remove elements no longer present.
      cy.elements().forEach((el) => {
        if (!wanted.has(el.id())) el.remove();
      });
      // Add elements not yet present.
      for (const d of defs) {
        if (cy.getElementById(d.data.id as string).empty()) cy.add(d);
      }
    });

    const initial = modeChanged;
    // Only relayout when topology actually changed (or on mode switch), so a
    // steady-state poll doesn't perturb the graph.
    const topoChanged = modeChanged || cy.elements().length !== defs.length;
    if (initial || topoChanged) {
      cy.layout(layoutOpts(initial)).run();
    }
  }, [elements, mode]);

  return <div className="graph" ref={containerRef} />;
}
