-- Which market-data source wins when several can price the same instrument.
--
-- Policy, not per-instrument data: ranking every instrument individually would be
-- ~42k copies of the same integer, and the answer is almost always a property of
-- the (source, asset class) pair rather than of one contract. A per-instrument
-- override belongs on the xref row if a specific name ever needs one; nothing does
-- today.
--
-- rank is ordinal, lower = preferred. Gaps are deliberate so a source can be
-- slotted between two others without renumbering.
--
-- Why Binance outranks Bybit for SPOT: we route crypto orders to Binance, so its
-- book is the one our fills print against. Bybit prices the same pair within a few
-- bps (arbitrage), which makes it a good fallback but not the same book — using it
-- while Binance is healthy would value positions against a venue we do not trade.

CREATE TABLE provider_feed_policy (
    source_code      TEXT NOT NULL,
    instrument_class TEXT NOT NULL,
    rank             INTEGER NOT NULL,
    enabled          BOOLEAN NOT NULL DEFAULT true,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (source_code, instrument_class),
    CONSTRAINT provider_feed_policy_rank_positive CHECK (rank > 0)
);

COMMENT ON TABLE provider_feed_policy IS
    'Ranked market-data source preference per (source, instrument_class); lower rank wins.';
COMMENT ON COLUMN provider_feed_policy.rank IS
    'Ordinal preference, lower = preferred. A source is used only when every better-ranked source is stale or down.';

INSERT INTO provider_feed_policy (source_code, instrument_class, rank) VALUES
    ('DATABENTO', 'OPTION', 10),   -- only OPRA source; consolidated NBBO
    ('BINANCE',   'SPOT',   10),   -- the venue we trade
    ('BYBIT',     'SPOT',   20)    -- fallback: same pair, different book
ON CONFLICT (source_code, instrument_class) DO NOTHING;
