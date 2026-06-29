import { createTheme, type MantineColorsTuple } from "@mantine/core";

// openOMS brand: depth-ladder green ("bids"), JetBrains Mono, near-black surfaces.
const MONO = "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

const depth: MantineColorsTuple = [
  "#e6f8f1",
  "#cdeede",
  "#9fdcc0",
  "#6dca9f",
  "#46bb84",
  "#2fb274",
  "#22ae6c", // 6 — primary
  "#149a5b",
  "#04894f",
  "#007541",
];

// Brand-aligned dark scale (#0d1014 body, #1a1e26 elevated, #2a2f38 borders).
const ink: MantineColorsTuple = [
  "#c9cdd4",
  "#9aa3af",
  "#6b7280",
  "#4b5563",
  "#374151",
  "#2a2f38",
  "#1a1e26",
  "#14181d",
  "#0d1014",
  "#080a0d",
];

export const theme = createTheme({
  fontFamily: MONO,
  fontFamilyMonospace: MONO,
  headings: { fontFamily: MONO, fontWeight: "700" },
  primaryColor: "depth",
  primaryShade: { light: 6, dark: 6 },
  defaultRadius: "sm",
  colors: { depth, dark: ink },
  other: {
    brandBg: "#0d1014",
    brandInk: "#eff1f4",
    brandMuted: "#9aa3af",
    amber: "#ce9a3b",
  },
});
