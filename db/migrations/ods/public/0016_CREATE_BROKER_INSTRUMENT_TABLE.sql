-- Broker symbology + execution mapping: master instrument_id <-> a broker's own
-- symbol/handle (Alpaca, IBKR, Binance). Re-introduces broker_instrument as the
-- authoritative home for the broker half of the mapping, replacing the source_type
-- discriminated oms.instrument_xref (dropped in a later migration once routing and
-- the resolver read this table instead).
--
-- This is also the *seeding* source of truth: broker sync (`oms setup sync-broker`)
-- upserts one row per tradeable instrument the broker offers, having already created
-- the master public.instrument row in the same pass. So a row here means "this
-- broker can route this instrument", and its existence is what makes an instrument
-- tradeable — there is no priceable-but-not-tradeable path.

CREATE TABLE broker_instrument (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instrument_id   BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    broker_code     TEXT NOT NULL,               -- ALPACA | IBKR | BINANCE
    broker_symbol   TEXT NOT NULL,               -- the broker's own handle (Alpaca ticker, compact OSI, pair)
    broker_exchange TEXT,                         -- the broker's exchange label, when it exposes one
    -- Broker-native instrument id used for order routing. broker_symbol/exchange are
    -- the human-facing mapping; the value we actually route on is the broker's own
    -- instrument handle (IBKR conId, Alpaca asset UUID, …): instrument_id ->
    -- broker_instrument[broker] -> native_id -> broker API. Nullable — options route
    -- by symbol and expose no stable native id.
    native_id       TEXT,
    is_tradeable    BOOLEAN NOT NULL DEFAULT true,
    -- Broker-enforced order limits (vary per broker; the binding constraint at routing time).
    min_quantity    NUMERIC,
    max_quantity    NUMERIC,
    min_notional    NUMERIC,
    max_notional    NUMERIC,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (instrument_id, broker_code),
    -- The routing lookup key: broker_symbol is how order entry addresses the
    -- instrument. Unique per (broker, symbol, exchange) — a consolidated broker can
    -- list the same symbol on several venues.
    --
    -- native_id is deliberately NOT unique: for Alpaca it is a per-instrument asset
    -- UUID, but for Binance it is the base asset, which repeats across quote pairs
    -- (BTCUSDT and BTCUSDC both base BTC). It is a routing/recon attribute, not a key.
    CONSTRAINT broker_instrument_broker_symbol_uq UNIQUE (broker_code, broker_symbol, broker_exchange),
    CHECK (max_quantity IS NULL OR min_quantity IS NULL OR max_quantity >= min_quantity),
    CHECK (max_notional IS NULL OR min_notional IS NULL OR max_notional >= min_notional)
);

CREATE INDEX idx_broker_instrument_broker     ON broker_instrument(broker_code);
CREATE INDEX idx_broker_instrument_instrument ON broker_instrument(instrument_id);

COMMENT ON TABLE broker_instrument IS
    'Broker symbology + execution mapping: master instrument_id <-> a broker''s handle. Seeded by broker sync; its existence makes an instrument tradeable.';
COMMENT ON COLUMN broker_instrument.native_id IS
    'Broker-native instrument id used for order routing (e.g. IBKR conId, Alpaca asset UUID).';
