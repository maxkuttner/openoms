-- Reported strategies (path 3): a strategy that trades elsewhere and reports its
-- fills to the OMS over HTTP. It has no FIX session, so no service principal or
-- comp ids. Reporting tokens authenticate it; each reported batch is kept for
-- idempotent replay; each fill is deduped on its venue execution id. Fills that
-- cannot be booked become recon breaks with source REPORTED.

-- Strategy: REPORTED mode, FIX identity only required for the other modes.
ALTER TABLE strategy DROP CONSTRAINT strategy_mode_check;
ALTER TABLE strategy ADD CONSTRAINT strategy_mode_check CHECK (mode IN ('VIA_OMS', 'COLOCATED', 'REPORTED'));
ALTER TABLE strategy
    ALTER COLUMN principal_id DROP NOT NULL,
    ALTER COLUMN sender_comp_id DROP NOT NULL,
    ALTER COLUMN target_comp_id DROP NOT NULL,
    ADD COLUMN stale_after_secs INT NOT NULL DEFAULT 300,
    ADD CONSTRAINT strategy_identity_check CHECK (
        mode = 'REPORTED'
        OR (principal_id IS NOT NULL AND sender_comp_id IS NOT NULL AND target_comp_id IS NOT NULL)),
    ADD CONSTRAINT strategy_stale_after_secs_check CHECK (stale_after_secs > 0);

-- A group may be bound to one strategy (the reported strategy's viewers).
ALTER TABLE user_group ADD COLUMN strategy_code TEXT UNIQUE REFERENCES strategy(code);

CREATE TABLE strategy_reporting_token (
    id            UUID PRIMARY KEY,
    strategy_code TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at    TIMESTAMPTZ,
    expires_at    TIMESTAMPTZ
);
CREATE INDEX idx_strategy_reporting_token_strategy ON strategy_reporting_token (strategy_code);

CREATE TABLE reported_batch (
    strategy_code TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    batch_id      TEXT NOT NULL,
    sequence      BIGINT,
    received_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    booked        INT NOT NULL DEFAULT 0,
    duplicate     INT NOT NULL DEFAULT 0,
    broken        INT NOT NULL DEFAULT 0,
    sequence_gap  BOOLEAN NOT NULL DEFAULT false,
    response_json JSONB NOT NULL,
    payload_json  JSONB NOT NULL,
    PRIMARY KEY (strategy_code, batch_id)
);

CREATE TABLE reported_fill (
    id                UUID PRIMARY KEY,
    strategy_code     TEXT NOT NULL REFERENCES strategy(code) ON DELETE CASCADE,
    batch_id          TEXT NOT NULL,
    venue             TEXT NOT NULL,
    account_ref       TEXT NOT NULL,
    exec_id           TEXT NOT NULL,
    client_order_id   TEXT,
    symbol            TEXT NOT NULL,
    instrument_id     BIGINT NOT NULL REFERENCES public.instrument(id),
    side              TEXT NOT NULL,
    qty               NUMERIC NOT NULL,
    price             NUMERIC NOT NULL,
    fee               NUMERIC,
    fee_asset         TEXT,
    fee_effect        TEXT,
    trade_time        TIMESTAMPTZ NOT NULL,
    booked_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    raw_json          JSONB NOT NULL,
    UNIQUE (strategy_code, venue, account_ref, exec_id),
    FOREIGN KEY (strategy_code, batch_id) REFERENCES reported_batch (strategy_code, batch_id),
    CHECK (side IN ('BUY', 'SELL')),
    CHECK (qty > 0),
    CHECK (price > 0),
    CHECK (fee_effect IS NULL OR fee_effect IN ('Quote', 'Base', 'Other'))
);
CREATE INDEX idx_reported_fill_batch ON reported_fill (strategy_code, batch_id);

-- Recon breaks: RECONCILED (venue order the OMS cannot reconcile) or REPORTED
-- (a reported fill that could not be booked, kept whole for adoption).
ALTER TABLE recon_break
    ALTER COLUMN broker_connection_code DROP NOT NULL,
    ALTER COLUMN account_id DROP NOT NULL,
    ALTER COLUMN venue_order_id DROP NOT NULL,
    ADD COLUMN source TEXT NOT NULL DEFAULT 'RECONCILED',
    ADD CONSTRAINT recon_break_source_values CHECK (source IN ('RECONCILED', 'REPORTED')),
    ADD COLUMN strategy_code TEXT REFERENCES strategy(code),
    ADD COLUMN reported_fill_json JSONB,
    ADD CONSTRAINT recon_break_source_check CHECK (
        (source = 'RECONCILED'
            AND broker_connection_code IS NOT NULL AND account_id IS NOT NULL AND venue_order_id IS NOT NULL)
        OR (source = 'REPORTED'
            AND strategy_code IS NOT NULL AND reported_fill_json IS NOT NULL));

CREATE UNIQUE INDEX recon_break_reported_one_open
    ON recon_break (strategy_code, (reported_fill_json->>'venue'),
                    (reported_fill_json->>'account_ref'), (reported_fill_json->>'exec_id'))
    WHERE status = 'open' AND source = 'REPORTED';
