// Mirrors src/handlers.rs's MarkRow. A field is null when that half of the
// data (live mark, or previous close) hasn't been populated for this
// instrument yet — never a fabricated 0, so callers must not `?? 0` these.
export interface MarkRow {
  instrument_id: number;
  bid: number | null;
  ask: number | null;
  mid: number | null;
  prev_close: number | null;
  pct_change: number | null;
}

// Mirrors src/handlers.rs's VenueOption. `reason` is set only when
// `eligible` is false — never both null-checked independently, since they
// always agree by construction on the server.
export interface VenueOption {
  broker_code: string;
  environment: string | null;
  broker_connection_code: string | null;
  account_id: string | null;
  eligible: boolean;
  reason: string | null;
}
