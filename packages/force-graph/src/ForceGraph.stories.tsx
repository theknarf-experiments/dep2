import { useRef, useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import { ForceGraphCanvas } from "./ForceGraphCanvas";
import { GraphElements, Perf } from "./types";

// A deterministic sample graph: `groups` clusters of `perGroup` nodes, each
// wired to a hub plus a few cross-group links. Seeded so stories are stable.
function sample(groups: number, perGroup: number, opts?: { labelAll?: boolean }): GraphElements {
  let seed = 1337;
  const rand = () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed / 0x7fffffff;
  };
  const nodes: GraphElements["nodes"] = [];
  const edges: GraphElements["edges"] = [];
  for (let g = 0; g < groups; g++) {
    const group = `group-${g}`;
    const color = `hsl(${Math.round((g * 360) / groups)}, 65%, 58%)`;
    const hub = `${group}/hub`;
    nodes.push({ id: hub, label: group, color, group, radius: 10, alwaysLabel: true, fontSize: 7 });
    for (let i = 0; i < perGroup; i++) {
      const id = `${group}/n${i}`;
      nodes.push({ id, label: `n${i}`, color, group, radius: 4, alwaysLabel: opts?.labelAll });
      edges.push({ id: `${hub}->${id}`, source: hub, target: id });
      if (i > 0 && rand() < 0.4) {
        edges.push({ id: `${id}->prev${i}`, source: id, target: `${group}/n${i - 1}` });
      }
    }
    if (g > 0) edges.push({ id: `link-${g}`, source: hub, target: `group-${g - 1}/hub` });
  }
  return { nodes, edges };
}

// Stateful wrapper so hover/select work in the story like in a real app.
function Demo({ elements, activeGroup }: { elements: GraphElements; activeGroup?: string | null }) {
  const [hovered, setHovered] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const perf = useRef<Perf>({ fps: 0, worstMs: 0 });
  return (
    <ForceGraphCanvas
      elements={elements}
      hovered={hovered}
      setHovered={setHovered}
      selected={selected}
      setSelected={setSelected}
      activeGroup={activeGroup ?? null}
      perf={perf}
    />
  );
}

const meta: Meta<typeof Demo> = {
  title: "ForceGraph",
  component: Demo,
};
export default meta;

type Story = StoryObj<typeof Demo>;

export const Small: Story = {
  name: "Small (4 groups)",
  args: { elements: sample(4, 6, { labelAll: true }) },
};

export const Large: Story = {
  name: "Large (~300 nodes)",
  args: { elements: sample(12, 24) },
};

export const SpotlightGroup: Story = {
  name: "Spotlight a group",
  args: { elements: sample(5, 10), activeGroup: "group-2" },
};
