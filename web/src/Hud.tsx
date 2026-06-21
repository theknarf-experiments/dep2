// In-scene HUD built with @react-three/uikit — rendered inside the WebGL canvas,
// not as DOM. The Fullscreen root is pointer-transparent so the graph behind it
// still pans/zooms; only the interactive bars opt back into pointer events.

import { MutableRefObject } from "react";
import { Container, Fullscreen, Text } from "@react-three/uikit";
import { Mode } from "./model";

interface Props {
  mode: Mode;
  setMode: (m: Mode) => void;
  paused: boolean;
  togglePause: () => void;
  status: "connecting" | "live" | "paused";
  counts: { nodes: number; edges: number };
  groups: { name: string; color: string }[];
  controls: MutableRefObject<any>;
}

function Btn({
  active,
  onClick,
  children,
}: {
  active?: boolean;
  onClick: () => void;
  children: string;
}) {
  return (
    <Container
      onClick={onClick}
      cursor="pointer"
      paddingX={10}
      paddingY={5}
      borderRadius={6}
      backgroundColor={active ? "#ffd166" : "#1b1b21"}
      hover={{ backgroundColor: active ? "#ffd166" : "#27272f" }}
    >
      <Text fontSize={13} color={active ? "#1a1a1a" : "#cfcfd6"}>
        {children}
      </Text>
    </Container>
  );
}

const STATUS_COLOR: Record<Props["status"], string> = {
  connecting: "#ffd166",
  live: "#4ade80",
  paused: "#9a9aa4",
};

export function Hud({ mode, setMode, paused, togglePause, status, counts, groups, controls }: Props) {
  // Disable camera controls while the pointer is over the toolbar; otherwise
  // OrbitControls (native DOM listeners) pans on press, and the resulting
  // movement makes uikit cancel the click so the button never fires.
  const setControls = (enabled: boolean) => {
    if (controls.current) controls.current.enabled = enabled;
  };
  return (
    <Fullscreen flexDirection="column" justifyContent="space-between" padding={12} pointerEvents="none">
      {/* top bar */}
      <Container
        flexDirection="row"
        alignItems="center"
        gap={10}
        padding={8}
        borderRadius={10}
        backgroundColor="#16161a"
        pointerEvents="auto"
        alignSelf="flex-start"
        onPointerEnter={() => setControls(false)}
        onPointerLeave={() => setControls(true)}
      >
        <Text fontSize={15} fontWeight="bold" color="#ffffff">
          dep2
        </Text>
        <Text fontSize={12} color="#9a9aa4">
          live import graph
        </Text>
        <Container flexDirection="row" gap={4} marginLeft={6}>
          <Btn active={mode === "crate"} onClick={() => setMode("crate")}>
            Crates
          </Btn>
          <Btn active={mode === "file"} onClick={() => setMode("file")}>
            Files
          </Btn>
        </Container>
        <Btn onClick={togglePause}>{paused ? "Resume" : "Pause"}</Btn>
        <Text fontSize={12} color="#9a9aa4">
          {`${counts.nodes} nodes · ${counts.edges} edges`}
        </Text>
        <Container flexDirection="row" alignItems="center" gap={5}>
          <Container width={8} height={8} borderRadius={4} backgroundColor={STATUS_COLOR[status]} />
          <Text fontSize={12} color="#9a9aa4">
            {status}
          </Text>
        </Container>
      </Container>

      {/* legend (non-interactive) */}
      {groups.length > 0 && (
        <Container
          flexDirection="row"
          flexWrap="wrap"
          gap={8}
          maxWidth={640}
          padding={8}
          borderRadius={10}
          backgroundColor="#16161a"
          pointerEvents="none"
          alignSelf="flex-start"
        >
          {groups.map((g) => (
            <Container key={g.name} flexDirection="row" alignItems="center" gap={4}>
              <Container width={8} height={8} borderRadius={4} backgroundColor={g.color} />
              <Text fontSize={10} color="#b8b8c0">
                {g.name}
              </Text>
            </Container>
          ))}
        </Container>
      )}
    </Fullscreen>
  );
}
