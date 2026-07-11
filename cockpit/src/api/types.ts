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
  state: "connecting" | "live" | "down";
  connected_since: string | null;
  last_event_at: string | null;
  last_error: string | null;
}

export interface BrokerConnection {
  code: string;
  broker_code: string;
  environment: string;
  status: string;
  created_at: string;
  updated_at: string;
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

export interface ReconBreak {
  instrument_id: string | null;
  symbol: string | null;
  oms_qty: number;
  custodian_qty: number;
  diff: number;
  kind: string;
}

export interface ReconSummary {
  run_id: string;
  broker_connection_code: string;
  oms_count: number;
  custodian_count: number;
  break_count: number;
  breaks: ReconBreak[];
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

export interface UniverseSummary {
  code: string;
  description: string | null;
  provider_code: string;
  category: string;
  dataset: string;
  option_dataset: string | null;
  include_options: boolean;
  status: string;
  last_seeded_at: string | null;
  last_error: string | null;
  instrument_count: number | null;
}

export interface EstimateResponse {
  universe_code: string;
  usd: number;
  symbol_count: number | null;
}

export interface UnderlyingCandidate {
  symbol: string;
  name: string;
  venue: string;
}
