-- Named instrument universes: the user-selectable sets of instruments the
-- Databento seeder loads. A universe is dataset-centric — one Databento dataset
-- plus an optional list of constituent symbols (instrument_universe_symbol).
-- An empty symbol list means "seed the whole dataset" (ALL_SYMBOLS); a non-empty
-- list narrows it to those symbols (e.g. an index's constituents).
--
-- Seeded with the main Databento datasets, all disabled — the user opts in via
-- the seeder's interactive picker. This table is both the catalog of available
-- universes and the record of which are enabled and when each was last seeded;
-- the seeder (scripts/seed_instruments.py) reads enabled rows and writes the
-- seed-state columns back.

CREATE TABLE instrument_universe (
    -- code is the natural key: stable, human-facing, and how the seeder + child
    -- table reference a universe. No surrogate id needed for a small catalog.
    code             TEXT PRIMARY KEY,                   -- XNAS_ITCH, OPRA, SPY_OPTIONS
    description      TEXT,
    provider_code    TEXT NOT NULL DEFAULT 'DATABENTO',
    category         TEXT NOT NULL DEFAULT 'EQUITY',     -- the dataset's instrument category
    dataset          TEXT NOT NULL,                      -- Databento dataset, e.g. XNAS.ITCH
    option_dataset   TEXT,                               -- OPRA chain for equity universes (nullable)
    stype_in         TEXT NOT NULL DEFAULT 'raw_symbol', -- input symbology for child symbols
    include_options  BOOLEAN NOT NULL DEFAULT false,     -- only honored with explicit symbols
    enabled          BOOLEAN NOT NULL DEFAULT false,     -- selected for seeding (opt-in)
    -- Seed state, written by the loader.
    status           TEXT NOT NULL DEFAULT 'PENDING',    -- PENDING|SEEDING|SEEDED|ERROR
    last_seeded_at   TIMESTAMPTZ,
    last_error       TEXT,
    instrument_count INTEGER,                            -- instruments upserted on last run
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (category IN ('EQUITY','OPTION','FUTURE')),
    CHECK (status IN ('PENDING','SEEDING','SEEDED','ERROR'))
);

CREATE INDEX idx_instrument_universe_enabled ON instrument_universe(enabled);

COMMENT ON TABLE instrument_universe IS
    'Catalog + seed-state of named instrument universes loaded by the Databento seeder.';
COMMENT ON COLUMN instrument_universe.category IS
    'Instrument category the dataset holds (EQUITY|OPTION|FUTURE). v1 seeding supports EQUITY.';
COMMENT ON COLUMN instrument_universe.stype_in IS
    'Databento input symbology for the universe''s child symbols (e.g. raw_symbol).';
COMMENT ON COLUMN instrument_universe.include_options IS
    'Seed the OPRA option chain per child symbol. Only honored when the universe has explicit symbols (OPRA is per-underlying).';
