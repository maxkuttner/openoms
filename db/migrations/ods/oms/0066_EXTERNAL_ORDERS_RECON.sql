-- External orders and execution reconciliation: per-connection recon settings,
-- attribution rules, the venue-fill dedup ledger, recon cursors and recon breaks.
-- One order_state row per venue order per account, so a venue order id is unique.

ALTER TABLE broker_connection
    ADD COLUMN default_portfolio_id UUID REFERENCES portfolio(id),
    ADD COLUMN external_policy TEXT NOT NULL DEFAULT 'adopt',
    ADD COLUMN eod_lookback TEXT NOT NULL DEFAULT '1w',
    ADD COLUMN intraday_poll_secs INT,                    -- NULL = the adapter's own hint
    -- Pulls and sweeps drop fills executed before this: set below for a connection
    -- whose pre-upgrade fills cannot all be re-keyed to today's exec ids.
    ADD COLUMN recon_not_before TIMESTAMPTZ,
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
    -- Who manages the rule: NULL for a person (API/cockpit), 'colo:<strategy>'
    -- for the rule a co-located registration sets. Registration touches only its own.
    owner                  TEXT,
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
-- executions does not book them twice. The ids are re-keyed to what today's
-- ingest paths report:
--   IBKR_TWS        execId without its trailing `.NN` revision
--   ALPACA          "{order id}:{cum qty after the fill}"
--   BINANCE, OKX    "{symbol}:{trade id}:{B|S}" (trade ids are per symbol)
--   others          unchanged
CREATE TEMP TABLE legacy_fill ON COMMIT DROP AS
SELECT o.broker_connection_code, o.account_id, o.order_id, e.created_at, bc.broker_code,
       e.payload_json->'payload'->>'execution_id' AS old_id,
       CASE bc.broker_code
         WHEN 'IBKR_TWS' THEN regexp_replace(e.payload_json->'payload'->>'execution_id', '\.[^.]*$', '')
         WHEN 'ALPACA' THEN
           CASE WHEN o.external_order_id IS NOT NULL AND jsonb_typeof(e.payload_json->'payload'->'cum_qty') = 'number'
                THEN o.external_order_id || ':' || trim_scale((e.payload_json->'payload'->>'cum_qty')::numeric)::text END
         WHEN 'BINANCE' THEN sym.broker_symbol || ':' || (e.payload_json->'payload'->>'execution_id') || ':'
                             || CASE WHEN upper(o.side) = 'BUY' THEN 'B' ELSE 'S' END
         WHEN 'OKX' THEN sym.broker_symbol || ':' || (e.payload_json->'payload'->>'execution_id') || ':'
                         || CASE WHEN upper(o.side) = 'BUY' THEN 'B' ELSE 'S' END
         ELSE e.payload_json->'payload'->>'execution_id'
       END AS new_id
FROM order_event e
JOIN order_state o ON o.order_id = e.order_id
JOIN broker_connection bc ON bc.code = o.broker_connection_code
LEFT JOIN LATERAL (
    SELECT bi.broker_symbol FROM public.broker_instrument bi
    WHERE bi.broker_code = bc.broker_code AND bi.instrument_id::text = o.instrument_id
    ORDER BY bi.broker_symbol LIMIT 1
) sym ON true
WHERE e.event_type IN ('order_partially_filled', 'order_filled')
  AND e.payload_json->'payload'->>'execution_id' IS NOT NULL;

INSERT INTO venue_fill (broker_connection_code, account_id, exec_id, order_id, booked_at)
SELECT broker_connection_code, account_id, COALESCE(new_id, old_id), order_id, created_at
FROM legacy_fill
ON CONFLICT DO NOTHING;

-- Where a legacy id cannot be re-keyed (Binance FIX keyed fills on ExecID(17),
-- indistinguishable from a trade id; a fill whose symbol or cum qty is unknown),
-- a re-read would book it again: the connection's pulls ignore anything
-- executed before the upgrade.
UPDATE broker_connection bc SET recon_not_before = now()
WHERE EXISTS (
    SELECT 1 FROM legacy_fill l
    WHERE l.broker_connection_code = bc.code AND (l.broker_code = 'BINANCE' OR l.new_id IS NULL)
);
