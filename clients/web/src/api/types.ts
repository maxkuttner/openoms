import type { FeeAmount } from "../fees";
// Hand-written mirror of the admin API resource shapes (src/admin.rs, src/handlers.rs).
// Could be generated from the OpenAPI doc later (openapi-typescript) to avoid drift.

export interface Portfolio {
  id: string;
  code: string;
  name: string;
  status: string;
  base_currency: string | null;
  created_at: string;
  updated_at: string;
}

export interface Account {
  id: string;
  code: string;
  broker_connection_code: string;
  external_account_ref: string;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface StreamHealth {
  broker_code: string;
  environment: string;
  kind: "feed" | "execution";
  transport: "websocket" | "fix";
  state: "connecting" | "live" | "down";
  connected_since: string | null;
  last_event_at: string | null;
  last_error: string | null;
}

export interface FeedSummary {
  feed_code: string;
  instrument_class: string;
  rank: number;
  enabled: boolean;
  mapped_instruments: number;
}

export interface BrokerConnection {
  code: string;
  broker_code: string;
  environment: string;
  status: string;
  created_at: string;
  updated_at: string;
}

// A configured feed connection row (distinct from FeedSummary, which is the
// ranked provider/instrument-class policy). Mirrors admin.rs's
// FeedConnectionSummary.
export interface FeedConnectionSummary {
  code: string;
  provider: string;
  dataset: string | null;
  status: string;
  created_at: string;
  updated_at: string;
}

// --- Credentials (src/admin.rs, src/credentials.rs) -----------------------
//
// Shared by broker-connections and feed-connections: GET/PUT/DELETE
// .../credentials and POST .../credentials/test all speak these shapes.

export interface RedactedField {
  name: string;
  /**
   * The stored value, when it is safe to echo and safe to submit back
   * unchanged. Null for both secrets and masked identifiers — in either case
   * the input must be left blank, so an untouched field submits empty and the
   * server's merge rule keeps what is stored.
   */
  value: string | null;
  secret: boolean;
  /** Display-only preview of a withheld value. Never send this back. */
  hint: string | null;
}

export interface RedactedCredentials {
  code: string;
  state: "configured" | "unconfigured" | "error";
  /** Populated only when state === "configured". */
  fields: RedactedField[];
  /** Set only when state === "error". */
  message: string | null;
  updated_at: string | null;
}

export interface TestResponse {
  tested: boolean;
  ok: boolean;
  message: string | null;
}

// Serde external tagging on the Rust side (reload::ConnectionOutcome): unit
// variants serialize as bare strings, the one payload-carrying variant as an
// object — `{ Failed: "reason" }`.
export type ConnectionOutcome =
  | "Registered"
  | "Unconfigured"
  | "Disabled"
  | "RestartRequired"
  | { Failed: string };

export interface SaveResponse {
  redacted: RedactedCredentials;
  /** false for FIX (IbkrFix/BinanceFix) — never checked before writing. */
  tested: boolean;
  /** Present exactly when tested === false. */
  message: string | null;
  /** null only if the reload report unexpectedly lacked an entry for this code. */
  reload: ConnectionOutcome | null;
}

export interface ApiKeyRecord {
  id: string;
  principal_id: string;
  key_id: string;
  name: string | null;
  created_at: string;
  secret?: string; // returned once on creation
}

export interface Grant {
  id: string;
  principal_id: string;
  portfolio_id: string;
  can_trade: boolean;
  can_view: boolean;
  can_allocate: boolean;
  created_at: string;
  updated_at: string;
}

export interface RiskLimit {
  id: string;
  portfolio_id: string;
  instrument_id: string;
  trading_state: string;
  max_order_quantity: number | null;
  max_order_notional: number | null;
  max_position_quantity: number | null;
  max_position_notional: number | null;
  created_at: string;
  updated_at: string;
}


export interface BlotterRow {
  order_id: string;
  client_order_id: string;
  principal_id: string;
  principal_code: string;
  portfolio_id: string;
  portfolio_code: string;
  account_id: string;
  broker_connection_code: string;
  instrument_id: string;
  instrument_symbol: string | null;
  instrument_name: string | null;
  side: string;
  order_type: string;
  status: string;
  original_qty: number;
  leaves_qty: number;
  cum_qty: number;
  avg_px: number | null;
  fees: FeeAmount[];
  created_at: string;
  /** When the order was placed; the first sighting when `placed_is_first_seen`. */
  placed_at: string;
  placed_is_first_seen: boolean;
  /** Venue execution time; null when nothing has executed. */
  trade_time: string | null;
  /** When the OMS booked it. */
  booked_at: string;
  /** True when the venue sent no execution time and trade_time is the booking time. */
  trade_time_estimated: boolean;
  updated_at: string;
  /** Set on a leg of a futures roll: the roll's parent (spread) order. */
  parent_order_id?: string | null;
}

/** One entry in an order's audit trail (GET /admin/orders/:id/events). */
export interface OrderEvent {
  version: number;
  event_id: string;
  event_type: string;
  actor: string;
  /** When the domain decided the event. */
  occurred_at: string;
  /** When the store appended it — later than `occurred_at` for broker-driven events. */
  recorded_at: string;
  /** When the store appended it (same instant as `recorded_at`). */
  booked_at: string;
  status_after: string | null;
  correlation_id: string | null;
  causation_id: string | null;
  schema_version: number;
  summary: string;
  payload: unknown;
}

export interface InstrumentSummary {
  id: number;
  symbol: string;
  name: string | null;
  venue: string;
  asset_class: string;
  status: string;
}

/** One listed futures contract of a root (GET /futures/roots). */
export interface FutureContract {
  instrument_id: number;
  symbol: string;
  expiry_date: string;
  roll_date: string;
  contract_size: number;
}

/** GET /futures/roots. `front`/`next` are null when the root lists no such contract. */
export interface FutureRoot {
  code: string;
  name: string;
  venue: string;
  roll_warn_days: number;
  /** Price increment of the root's calendar spreads, i.e. a roll's limit price (ES: 0.05). */
  spread_tick: number;
  front: FutureContract | null;
  next: FutureContract | null;
}

/** GET /futures/roll-targets: a held futures position and the contract it rolls into. */
export interface RollTargetRow {
  portfolio_id: string;
  instrument_id: number;
  symbol: string;
  root: string;
  net_qty: number;
  roll_date: string;
  roll_warn_days: number;
  next: { instrument_id: number; symbol: string };
}

/** GET /alerts; `payload` is kind-specific (ROLL_DUE: see RollDuePayload). */
export interface AlertRow {
  id: string;
  kind: string;
  portfolio_id: string;
  instrument_id: number | null;
  payload: Record<string, unknown>;
  created_at: string;
}

/** POST /orders/roll */
export interface RollRequest {
  portfolio_id: string;
  from_instrument_id: number;
  to_instrument_id: number;
  qty: number;
  order_type: "market" | "limit";
  limit_price?: number;
  time_in_force: "day";
}
export interface RollIds {
  parent_order_id: string;
  child_order_ids: string[];
}
