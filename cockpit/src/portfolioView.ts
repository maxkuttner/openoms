// Pure logic for the Portfolios and Orders pages.

/** The portfolio a page opens on: the first one the user may view, or null. */
export function pickDefaultPortfolio(portfolios: { portfolio_id: string; can_view?: boolean }[]): string | null {
  return portfolios.find((p) => p.can_view !== false)?.portfolio_id ?? null;
}
