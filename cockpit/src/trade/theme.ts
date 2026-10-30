import { createTheme } from "@mantine/core";
import { scales } from "../palette";

// The trader app's own theme — deliberately separate from ../theme (the admin
// console's). Same shape and semantics (a `depth`/`offer` pair for the book's
// two sides, `dark` for surfaces) so components written against either theme
// read the same way. Its own fonts and radii; its colours, like the console's,
// come from ../palette.ts, with orange as the accent.
const DISPLAY = "'Space Grotesk', -apple-system, sans-serif";
const BODY = "'IBM Plex Sans', system-ui, sans-serif";
const MONO = "'IBM Plex Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

export const theme = createTheme({
  fontFamily: BODY,
  fontFamilyMonospace: MONO,
  headings: { fontFamily: DISPLAY, fontWeight: "600" },
  primaryColor: "brand",
  primaryShade: { light: 6, dark: 6 },
  defaultRadius: "sm",
  radius: { xs: "4px", sm: "6px", md: "8px", lg: "12px", xl: "20px" },
  colors: { depth: scales.depth, offer: scales.offer, brand: scales.orange, amber: scales.amber, dark: scales.dark },
});
