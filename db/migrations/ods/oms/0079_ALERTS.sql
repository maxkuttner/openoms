-- Generic alert channel: something an operator should look at (first user: a held
-- futures contract nearing its roll date). One row per alert; `dedupe_key` makes
-- raising idempotent. Unpublished (published_at IS NULL) rows wait for the Kafka
-- bridge, so no separate outbox table is needed.

CREATE TABLE alert (
    id              UUID PRIMARY KEY,
    kind            TEXT NOT NULL,
    portfolio_id    UUID NOT NULL REFERENCES portfolio(id),
    instrument_id   BIGINT,
    dedupe_key      TEXT NOT NULL UNIQUE,
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    acknowledged_at TIMESTAMPTZ,
    published_at    TIMESTAMPTZ
);

CREATE INDEX alert_open_idx ON alert (portfolio_id, created_at DESC) WHERE acknowledged_at IS NULL;
CREATE INDEX alert_unpublished_idx ON alert (created_at) WHERE published_at IS NULL;
