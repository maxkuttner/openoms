import type { CSSProperties } from "react";

/// Shared table treatment for the trade screens, so the blotter and positions
/// read as one product rather than two tables that happen to sit in one app.

/// Numbers in a column are compared down the column, not read as prose: they
/// align right, and `tabular-nums` holds every digit to the same advance width
/// so the decimal points stack. JetBrains Mono is already monospaced, but the
/// fallback stack is not, and a column that jitters when the font fails to load
/// is worse than one that never aligned.
export const numeric: CSSProperties = {
  textAlign: "right",
  fontVariantNumeric: "tabular-nums",
};

/// Column headers are labels for a grid, not sentences. Small, uppercase and
/// letter-spaced is the convention every trading screen already uses, and it
/// buys vertical space back for rows.
export const columnHeader: CSSProperties = {
  textTransform: "uppercase",
  letterSpacing: "0.05em",
  fontSize: 11,
  fontWeight: 600,
  color: "var(--mantine-color-dimmed)",
};

/// The book's own pair: `depth` for bids, `offer` for asks. Used wherever a
/// side is shown, so the colour means the same thing on every screen.
export function sideColor(side: string): string {
  return side === "buy" ? "depth" : "offer";
}
