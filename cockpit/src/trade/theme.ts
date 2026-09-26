import { createTheme, type MantineColorsTuple } from "@mantine/core";

// The trader app's own theme — deliberately separate from ../theme (the admin
// console's). Same shape and semantics (a `depth`/`offer` pair for the book's
// two sides, `dark` for surfaces) so components written against either theme
// read the same way, but every value below comes from the trader-facing
// design mockup, not the admin console's.
const DISPLAY = "'Space Grotesk', -apple-system, sans-serif";
const BODY = "'IBM Plex Sans', system-ui, sans-serif";
const MONO = "'IBM Plex Mono', ui-monospace, SFMono-Regular, Menlo, monospace";

// Bid side. Index 6 is the working shade referenced directly as "depth"
// throughout the trade app (SideSelector, P&L sign, the Confirm-buy button).
const depth: MantineColorsTuple = [
  "#E8FBEF",
  "#C6F6D9",
  "#93E9B3",
  "#64DC91",
  "#3FCE76",
  "#29C766",
  "#22C55E", // 6 — primary
  "#189C48",
  "#107537",
  "#0A5227",
];

// Ask side — offer's twin, same construction, opposite hue.
const offer: MantineColorsTuple = [
  "#FDEBEA",
  "#FBD0CE",
  "#F6A29D",
  "#F37B74",
  "#F15F56",
  "#F04F46",
  "#F0453D", // 6 — primary
  "#C6362F",
  "#9C2A24",
  "#721E1A",
];

// Neutral interactive accent — tabs, focus rings, selection, the logo mark.
// Deliberately not used for buy/sell so a selected row and a buy order are
// never the same color.
const brand: MantineColorsTuple = [
  "#EAF1FF",
  "#CFE0FF",
  "#A3C4FF",
  "#7AABFF",
  "#649EFF",
  "#588FFF",
  "#4C8DFF", // 6 — primary
  "#3B72D6",
  "#2C5AAD",
  "#1E4384",
];

// Partial-fill / warning amber, replacing Mantine's stock orange so
// `STATUS_COLOR.canceled` (TradeBlotter.tsx) and the "no broker configured"
// notifications (OrderTicket.tsx) land on the mockup's actual amber.
const orange: MantineColorsTuple = [
  "#FDF3E7",
  "#FBE2C2",
  "#F6C783",
  "#F3B156",
  "#F1A438",
  "#F0A623",
  "#F0A623", // 6 — primary
  "#C6821A",
  "#9C6414",
  "#724A0E",
];

// Surfaces. Index 5 is the border/hover shade, 6 the elevated panel bg, 9 the
// deepest body bg — the same roles ../theme.ts assigns its own `dark` scale,
// so `var(--mantine-color-dark-5)` etc. in existing components still mean
// the same thing.
const dark: MantineColorsTuple = [
  "#E7EAF0",
  "#C7CCD8",
  "#8A93A6",
  "#525C70",
  "#3B4456",
  "#232A38",
  "#1A2029",
  "#141A25",
  "#10141C",
  "#0A0D13",
];

export const theme = createTheme({
  fontFamily: BODY,
  fontFamilyMonospace: MONO,
  headings: { fontFamily: DISPLAY, fontWeight: "600" },
  primaryColor: "brand",
  primaryShade: { light: 6, dark: 6 },
  defaultRadius: "sm",
  radius: { xs: "4px", sm: "6px", md: "8px", lg: "12px", xl: "20px" },
  colors: { depth, offer, brand, orange, dark },
});
