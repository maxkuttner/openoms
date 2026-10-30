import { createTheme } from "@mantine/core";
import { scales } from "./palette";

// openOMS brand: depth-ladder green ("bids"), JetBrains Mono, black surfaces.
// Every colour comes from palette.ts.
const MONO = "'JetBrains Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

export const theme = createTheme({
  fontFamily: MONO,
  fontFamilyMonospace: MONO,
  headings: { fontFamily: MONO, fontWeight: "700" },
  primaryColor: "depth",
  primaryShade: { light: 6, dark: 6 },
  defaultRadius: "sm",
  colors: { depth: scales.depth, offer: scales.offer, amber: scales.amber, brand: scales.orange, dark: scales.dark },
});
