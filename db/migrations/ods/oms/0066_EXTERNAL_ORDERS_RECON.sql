-- External orders and execution reconciliation: per-connection recon settings,
-- attribution rules, the venue-fill dedup ledger, recon cursors and recon breaks.
-- One order_state row per venue order per account, so a venue order id is unique.

ALTER TABLE broker_connection
    ADD COLUMN default_portfolio_id UUID REFERENCES portfolio(id),
    ADD COLUMN external_policy TEXT NOT NULL DEFAULT 'adopt',
    ADD COLUMN eod_lookback TEXT NOT NULL DEFAULT '1w',
    ADD COLUMN intraday_poll_secs INT,                    -- NULL = the adapter's own hint
    ADD CONSTRAINT broker_connection_external_policy_check CHECK (external_policy IN ('adopt','break')),
    ADD CONSTRAINT broker_connection_eod_lookback_check CHECK (eod_lookback IN ('1d','1w','1m','1y','5y','10y'));

-- Which portfolio an adopted external order books into: first match by priority
-- (lowest number wins), else the connection's default_portfolio_id.
CREATE TABLE attribution_rule (
    id                     UUID PRIMARY KEY,
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    account_id             UUID REFERENCES account(id),
    client_id_prefix       TEXT,
    portfolio_id           UUID NOT NULL REFERENCES portfolio(id),
    priority               INT NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (account_id IS NOT NULL OR client_id_prefix IS NOT NULL)
);
CREATE INDEX idx_attribution_rule_connection ON attribution_rule(broker_connection_code, priority);

-- Every execution booked against an order, keyed by the venue's execution id. The
-- primary key is what makes booking a fill idempotent across ingest paths.
CREATE TABLE venue_fill (
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    account_id             UUID NOT NULL REFERENCES account(id),
    exec_id                TEXT NOT NULL,
    order_id               UUID NOT NULL REFERENCES order_state(order_id),
    booked_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (broker_connection_code, account_id, exec_id)
);
CREATE INDEX idx_venue_fill_order ON venue_fill(order_id);

-- Where each recon scope last read up to, per connection and account.
CREATE TABLE recon_cursor (
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    account_id             UUID NOT NULL REFERENCES account(id),
    scope                  TEXT NOT NULL,
    cursor                 TEXT NOT NULL,
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (broker_connection_code, account_id, scope)
);

-- A venue order the OMS could not reconcile on its own, for a person to resolve.
CREATE TABLE recon_break (
    id                     UUID PRIMARY KEY,
    broker_connection_code TEXT NOT NULL REFERENCES broker_connection(code),
    account_id             UUID NOT NULL REFERENCES account(id),
    venue_order_id         TEXT NOT NULL,
    kind                   TEXT NOT NULL,
    events_json            JSONB NOT NULL DEFAULT '[]'::jsonb,
    status                 TEXT NOT NULL DEFAULT 'open',
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at            TIMESTAMPTZ,
    resolved_by            UUID REFERENCES principal(id),
    CHECK (status IN ('open','resolved','dismissed'))
);
CREATE UNIQUE INDEX recon_break_one_open
    ON recon_break (broker_connection_code, account_id, venue_order_id) WHERE status = 'open';

-- Legacy colo ingest booked one EXTERNAL order per venue trade, so several rows
-- can share one venue order id. They are left in place (positions already count
-- them) and excluded from the index via venue_trade_id, which only colo sets.
-- Their fills are seeded into venue_fill below, so a later sweep sees their
-- exec_ids as duplicates and books nothing twice.
CREATE UNIQUE INDEX order_state_venue_order
    ON order_state (broker_connection_code, account_id, external_order_id)
    WHERE external_order_id IS NOT NULL AND venue_trade_id IS NULL;

-- Seed the ledger from fills already booked, so a re-read of the venue's
-- executions does not book them twice.
INSERT INTO venue_fill (broker_connection_code, account_id, exec_id, order_id, booked_at)
SELECT o.broker_connection_code, o.account_id, e.payload_json->'payload'->>'execution_id', o.order_id, e.created_at
FROM order_event e
JOIN order_state o ON o.order_id = e.order_id
WHERE e.event_type IN ('order_partially_filled', 'order_filled')
  AND e.payload_json->'payload'->>'execution_id' IS NOT NULL
ON CONFLICT DO NOTHING;
