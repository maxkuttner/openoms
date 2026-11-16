export type Side = "buy" | "sell";
export type OrderType = "market" | "limit";
export type TimeInForce = "day" | "gtc" | "ioc" | "fok";

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

// Mirrors src/admin.rs's InstrumentSummary (GET /instruments).
export interface SearchInstrument {
  id: number;
  symbol: string;
  name: string;
  venue: string;
  asset_class: string;
  instrument_class: string;
  status: string;
  has_options: boolean;
}

// Mirrors src/instruments_api.rs's ChainLeg/ChainRow (GET /instruments/options/chain).
export interface ChainLeg {
  instrument_id: number;
  symbol: string;
}
export interface ChainRow {
  strike: number;
  call: ChainLeg | null;
  put: ChainLeg | null;
}

// One leg of a strategy assembled in InstrumentSearchModal, handed to
// StrategyTicket on "Use this combo".
export interface StrategyLeg {
  instrumentId: string;
  symbol: string;
  optionKind: "CALL" | "PUT";
  strike: number;
  expiry: string; // YYYY-MM-DD
  side: Side;
  // The touched bid/ask at pick time, shown on the ticket for reference —
  // never sent to the server. submitOrder always trades at whatever
  // order type/price the ticket itself is set to.
  referencePrice: number | null;
}
