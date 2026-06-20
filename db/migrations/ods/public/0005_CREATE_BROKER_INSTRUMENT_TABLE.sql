-- Master data: master instrument_id <-> broker (Alpaca, IBKR) symbol mappings.

CREATE TABLE broker_instrument (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instrument_id   BIGINT NOT NULL REFERENCES instrument(id) ON DELETE CASCADE,
    broker_code     TEXT NOT NULL,
    broker_symbol   TEXT NOT NULL,
    broker_exchange TEXT,
    -- Broker-native instrument id used for order routing. broker_symbol/exchange
    -- are the human-facing mapping; the value we actually route on is the broker's
    -- own instrument handle (IBKR conId, Alpaca asset UUID, …), so the order path
    -- is instrument_id -> broker_instrument[broker] -> native_id -> broker API.
    -- Nullable: populated by the per-broker symbology sync, so rows can exist
    -- before the native id is resolved.
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
    -- Unique per broker so an inbound broker event (e.g. a fill referencing the
    -- native id) resolves back to exactly one instrument; NULLs are distinct in
    -- Postgres, so unsynced rows don't collide.
    CONSTRAINT broker_instrument_broker_native_uq UNIQUE (broker_code, native_id),
    CHECK (max_quantity IS NULL OR min_quantity IS NULL OR max_quantity >= min_quantity),
    CHECK (max_notional IS NULL OR min_notional IS NULL OR max_notional >= min_notional)
);

CREATE INDEX idx_broker_instrument_broker     ON broker_instrument(broker_code);
CREATE INDEX idx_broker_instrument_instrument ON broker_instrument(instrument_id);

COMMENT ON TABLE broker_instrument IS 'Master data — broker symbol mappings.';
COMMENT ON COLUMN broker_instrument.native_id
    IS 'Broker-native instrument id used for order routing (e.g. IBKR conId, Alpaca asset UUID).';
