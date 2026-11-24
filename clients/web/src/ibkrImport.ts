// Pure helpers for the "Import from IBKR" dialog (kept free of the API client
// so `npm test` can load them under plain node).

export interface Contract {
  con_id: number;
  symbol: string;
  primary_exchange: string;
  mic: string | null;
  currency: string;
  min_tick: number;
  lot_size: number | null;
  name: string;
  isin: string | null;
}

export interface ResolveRow {
  line: number;
  status: "found" | "ambiguous" | "not_found";
  candidates: Contract[];
  message: string;
}

export interface LoadReport {
  fetched: number;
  upserted: number;
  mapped: number;
  skipped: number;
  skipped_reasons: string[];
}

// Rows to save: every found row's contract, plus the candidate picked for an
// ambiguous row (picks maps CSV line -> chosen conId). Not-found never.
export function selectedConIds(rows: ResolveRow[], picks: Record<number, number>): number[] {
  const ids: number[] = [];
  for (const r of rows) {
    if (r.status === "found" && r.candidates.length > 0) ids.push(r.candidates[0].con_id);
    else if (r.status === "ambiguous") {
      const pick = picks[r.line];
      if (pick !== undefined && r.candidates.some((c) => c.con_id === pick)) ids.push(pick);
    }
  }
  return [...new Set(ids)];
}

export const GATEWAY_DOWN = "Start the IBKR connection first";

// Message for a failed call: the fixed hint for 503, the server's text verbatim otherwise.
export function importErrorMessage(status: number | undefined, message: string): string {
  return status === 503 ? GATEWAY_DOWN : message;
}
