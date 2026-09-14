// Hand-written mirror of the admin API resource shapes (src/admin.rs, src/handlers.rs).
// Could be generated from the OpenAPI doc later (openapi-typescript) to avoid drift.

export interface Principal {
  id: string;
  code: string;
  principal_type: string;
  external_subject: string | null;
  display_name: string | null;
  status: string;
  created_at: string;
  updated_at: string;
}

export interface Portfolio {
  id: string;
  code: string;
  name: string;
  status: string;
  base_currency: string | null;
  default_account_id: string | null;
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

// A single bearer token (Databento-style) minted under a principal. `token` once.
export interface TradingTokenCreated {
  token: string;
  key_id: string;
  principal_id: string;
  portfolio_id: string | null;
  label: string | null;
}

export interface TradingTokenRow {
  key_id: string;
  label: string | null;
  principal_id: string;
  principal_code: string;
  principal_name: string | null;
  created_at: string;
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
  created_at: string;
  updated_at: string;
}

export interface InstrumentSummary {
  id: number;
  symbol: string;
  name: string | null;
  venue: string;
  asset_class: string;
  status: string;
}
