/// Shared table treatment for the trade screens, so the blotter and positions
/// read as one product.

/// Numbers are compared down the column: right-aligned, monospace, tabular.
export const numeric = "num text-right";

/// The book's own pair: buy is the bid colour, sell the offer colour. Used
/// wherever a side is shown, so the colour means the same thing on every screen.
export function sideClass(side: string): string {
  return side === "buy" ? "text-buy" : "text-sell";
}
