-- Market-data mapping: a data feed's own symbol -> master instrument(s). The feed
-- half of what oms.instrument_xref conflated, given its own home and an explicit,
-- deliberately 1:n shape.
--
-- 1:n by design. One feed symbol may price several instruments: a BTC/USDT feed can
-- mark BTCUSDT on every venue that trades it, so the identity is the full
-- (feed_code, feed_symbol, instrument_id) triple, not (feed_code, feed_symbol).
-- Independent of brokers — a feed maps onto whatever instruments exist, regardless
-- of which broker seeded them.
--
-- Drives two things: subscription (instrument_id -> feed_symbol for held
-- instruments, in quote_feed) and inbound resolution (feed_symbol -> instrument_id,
-- when a quote arrives). Ranked failover across feeds stays in provider_feed_policy.

CREATE TABLE feed_instrument (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    feed_code     TEXT NOT NULL,                 -- DATABENTO | BINANCE | BYBIT
    feed_symbol   TEXT NOT NULL,                 -- the feed's own symbol (SOLUSDT, OSI, …)
    instrument_id BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    is_active     BOOLEAN NOT NULL DEFAULT true,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Full triple: one feed symbol -> many instruments is allowed; the same
    -- (feed, symbol, instrument) mapping is not duplicated.
    UNIQUE (feed_code, feed_symbol, instrument_id)
);

CREATE INDEX idx_feed_instrument_feed       ON feed_instrument(feed_code);
CREATE INDEX idx_feed_instrument_instrument ON feed_instrument(instrument_id);
CREATE INDEX idx_feed_instrument_lookup     ON feed_instrument(feed_code, feed_symbol);

COMMENT ON TABLE feed_instrument IS
    'Market-data mapping: a feed''s own symbol -> master instrument(s). 1:n; independent of brokers.';
