-- Durable OMS inbox for runtime event batches, one economic trade identity
-- across runtime and follow, and a native-asset movement ledger. Fees in a
-- third asset are preserved without FX conversion.

ALTER TABLE colo_deployment
    ADD COLUMN ingest_suspended_at TIMESTAMPTZ,
    ADD COLUMN ingest_suspended_reason TEXT;

CREATE TABLE colo_event_inbox (
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id) ON DELETE CASCADE,
    sequence      BIGINT NOT NULL,
    payload       JSONB NOT NULL,
    received_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at  TIMESTAMPTZ,
    PRIMARY KEY (deployment_id, sequence)
);
CREATE INDEX idx_colo_event_inbox_unprocessed ON colo_event_inbox(deployment_id, sequence)
    WHERE processed_at IS NULL;

CREATE TABLE colo_trade_ledger (
    id                UUID PRIMARY KEY,
    deployment_id     UUID NOT NULL REFERENCES colo_deployment(id),
    venue             TEXT NOT NULL,
    account_id        UUID NOT NULL,
    environment       TEXT NOT NULL,
    symbol            TEXT NOT NULL,
    trade_id          TEXT NOT NULL,
    venue_order_id    TEXT NOT NULL,
    client_order_id   TEXT NOT NULL,
    side              TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    quantity          NUMERIC NOT NULL CHECK (quantity > 0),
    price             NUMERIC NOT NULL CHECK (price > 0),
    executed_at       TIMESTAMPTZ NOT NULL,
    fee_amount        NUMERIC,
    fee_currency      TEXT,
    provenance        TEXT NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (venue, account_id, environment, symbol, trade_id),
    CHECK ((fee_amount IS NULL) = (fee_currency IS NULL))
);

-- Append-only native-asset movements. Gross instrument P&L stays separate from
-- fees, and a fee in a third asset moves that asset only.
CREATE TABLE colo_asset_event (
    id            UUID PRIMARY KEY,
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id),
    venue         TEXT NOT NULL,
    account_id    UUID NOT NULL,
    environment   TEXT NOT NULL,
    symbol        TEXT NOT NULL,
    trade_id      TEXT NOT NULL,
    asset         TEXT NOT NULL,
    entry_kind    TEXT NOT NULL CHECK (entry_kind IN ('BASE', 'QUOTE', 'FEE')),
    amount        NUMERIC NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (venue, account_id, environment, symbol, trade_id, asset, entry_kind)
);

CREATE TABLE colo_asset_balance (
    account_id UUID NOT NULL,
    asset      TEXT NOT NULL,
    quantity   NUMERIC NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, asset)
);

CREATE TABLE colo_baseline_adjustment (
    id            UUID PRIMARY KEY,
    deployment_id UUID NOT NULL REFERENCES colo_deployment(id),
    asset         TEXT NOT NULL,
    amount        NUMERIC NOT NULL,
    reason        TEXT NOT NULL,
    operator      TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
