-- Position projection (read model): the fund's net holding + cost basis + realized
-- P&L per (portfolio, instrument), maintained incrementally from fill events and
-- rebuildable from the order_event log. Not part of the event-sourced order core.
-- Unrealized P&L is intentionally absent — it needs a market-data mark (later).

CREATE TABLE position (
    portfolio_id   UUID NOT NULL REFERENCES portfolio(id),
    instrument_id  TEXT NOT NULL,                 -- master instrument id (as stored on order_state)
    net_qty        NUMERIC NOT NULL DEFAULT 0,    -- signed: + long, - short
    avg_cost       NUMERIC NOT NULL DEFAULT 0,    -- avg price of the OPEN position (>= 0; 0 when flat)
    realized_pnl   NUMERIC NOT NULL DEFAULT 0,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (portfolio_id, instrument_id)
);
