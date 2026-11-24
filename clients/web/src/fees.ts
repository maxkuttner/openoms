// Pure helper for the Fees / Other fees columns (kept free of React so
// `npm test` can load it under plain node).

// Mirrors FeeAmount in docs/openapi.json. A charged fee is positive, a rebate
// negative; amounts stay in the currency the venue charged them in.
export interface FeeAmount {
  currency: string;
  amount: number;
}

const amountFormat = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 2,
  maximumFractionDigits: 8,
  useGrouping: false,
});

/** "1.50 USD · 0.01 BNB", or "" when there are none. */
export function formatFees(fees: FeeAmount[] | null | undefined): string {
  return (fees ?? []).map((f) => `${amountFormat.format(f.amount)} ${f.currency}`).join(" · ");
}
