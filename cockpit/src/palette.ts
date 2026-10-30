import type { MantineColorsTuple } from "@mantine/core";

// The openoms colour scheme: the one list of colours the cockpit, the trader
// UI and the docs site are drawn from. It follows the landing page: pure black,
// a green and an orange glow, and the logo's green and amber.
//
// check-palette.sh fails the build on a hex colour anywhere else in src/. The
// docs site cannot import this file, so docs-site/src/styles/theme.css copies
// the named colours below as --oms-* tokens, and a docs test keeps them equal.
export const palette = {
  // Surfaces, darkest first. Dark mode is pure black.
  black: "#000000",
  surface: "#0d0d0d", // insets, inputs, code blocks
  raised: "#161616", // panels, headers, hover
  border: "#262626",

  // Text.
  ink: "#ededed",
  muted: "#a3a3a3",
  faint: "#737373",

  // Brand: the logo's two colours and the landing page's two glows.
  green: "#25b083", // logo, cockpit accent
  amber: "#ce9a3b", // logo; warnings and partial states
  orange: "#d97a14", // landing page's orange glow; trader UI accent
  glowGreen: "#148c47", // landing page's green glow

  // The book's two sides. Red also marks errors and offline states.
  buy: "#22ae6c",
  red: "#db3440",

  // Logo artwork (logo.svg, favicon.svg): the tile and the middle bar.
  logoTile: "#0d1014",
  logoBar: "#2a2f38",
} as const;

// Ten-step Mantine scales built around the colours above (index 6 is the
// working shade). Both UIs register them under the same names.
export const scales = {
  // Bid side and the cockpit's primary; 6 is palette.buy.
  depth: [
    "#e6f8f1", "#cdeede", "#9fdcc0", "#6dca9f", "#46bb84",
    "#2fb274", "#22ae6c", "#149a5b", "#04894f", "#007541",
  ] as MantineColorsTuple,

  // Ask side; 6 is palette.red.
  offer: [
    "#fdeced", "#f8d2d5", "#f0a6ab", "#e77a81", "#e0555e",
    "#dc3d48", "#db3440", "#c22530", "#a91b25", "#8f121a",
  ] as MantineColorsTuple,

  // Warnings, partial fills, interrupted jobs; 6 is palette.amber.
  amber: [
    "#fbf4e6", "#f5e5c4", "#ecd096", "#e2ba68", "#d9a94a",
    "#d3a040", "#ce9a3b", "#b3832d", "#966c22", "#7a5618",
  ] as MantineColorsTuple,

  // The trader UI's accent (tabs, focus, selection) — never buy or sell, so a
  // selected row and an order side are never the same colour; 6 is palette.orange.
  orange: [
    "#fdf0e3", "#fadbbb", "#f4bd85", "#ee9f4f", "#e88a2c",
    "#e2801c", "#d97a14", "#bb680f", "#9b560b", "#7c4408",
  ] as MantineColorsTuple,

  // Surfaces and text in Mantine's dark scheme: 0 text, 2 dimmed, 4–5 borders,
  // 6 inputs and panels, 7 the page.
  dark: [
    "#ededed", "#c4c4c4", "#a3a3a3", "#737373", "#3a3a3a",
    "#262626", "#161616", "#000000", "#000000", "#000000",
  ] as MantineColorsTuple,
};
