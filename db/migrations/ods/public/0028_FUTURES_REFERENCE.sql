-- Reference data for CME index futures traded through IBKR TWS.
--
--   * XCME           — the venue every future below lists on (ISO 10383 MIC), with a
--                      Chicago calendar closing 16:00 so the expiry sweep can turn a
--                      contract's bare expiry_date into an instant (see 0021/0022).
--   * future_root    — one row per futures root: how to ask the broker for it
--                      (broker_symbol/broker_exchange), which months it lists
--                      (contract_months, CME quarterly cycle H/M/U/Z) and when the
--                      one-click roll becomes due. Multipliers are deliberately not
--                      stored: they live on each contract (instrument.contract_size).
--   * future_spread  — links a calendar-spread instrument to its two legs.
--   * PERPETUAL      — a new instrument_class for perpetual swaps. SWAP stays valid.

-- Migrations run before the seed scripts on a fresh install, and future_root FKs to
-- currency, so USD must be here too (same row as seed_currencies.sql).
INSERT INTO currency (code, name, numeric_code, minor_units) VALUES ('USD', 'US Dollar', '840', 2)
ON CONFLICT (code) DO NOTHING;

-- The venue exists on installs seeded from the MIC registry; this covers the rest.
INSERT INTO venue (code, name, mic, status) VALUES ('XCME', 'Chicago Mercantile Exchange', 'XCME', 'ACTIVE')
ON CONFLICT (code) DO NOTHING;

INSERT INTO calendar (code, venue_id, timezone, close_time, description)
SELECT 'XCME', v.id, 'America/Chicago', TIME '16:00', 'CME Globex index futures'
FROM venue v WHERE v.code = 'XCME'
ON CONFLICT (code) DO NOTHING;

ALTER TABLE instrument DROP CONSTRAINT IF EXISTS instrument_instrument_class_check;
ALTER TABLE instrument ADD CONSTRAINT instrument_instrument_class_check
    CHECK (instrument_class IN ('SPOT','FUTURE','FORWARD','OPTION','SWAP','PERPETUAL','CFD','BOND','WARRANT'));

-- is_derivative is generated from instrument_class (0003) and must follow the CHECK:
-- a perpetual is a derivative. A generated expression cannot be altered in place, so
-- the column is rebuilt. Nothing else references it (no view, index or code).
ALTER TABLE instrument DROP COLUMN is_derivative;
ALTER TABLE instrument ADD COLUMN is_derivative BOOLEAN GENERATED ALWAYS AS
    (instrument_class IN ('FUTURE','FORWARD','OPTION','SWAP','PERPETUAL','CFD','WARRANT')) STORED;

CREATE TABLE future_root (
    code             TEXT PRIMARY KEY,                       -- root symbol, e.g. ES
    venue            TEXT NOT NULL REFERENCES venue(code),
    name             TEXT NOT NULL,
    asset_class      TEXT NOT NULL,
    currency         TEXT NOT NULL REFERENCES currency(code),
    broker_symbol    TEXT NOT NULL,                          -- symbol the broker knows the root by
    broker_exchange  TEXT NOT NULL,                          -- broker routing exchange (IBKR: CME)
    contract_months  TEXT NOT NULL,                          -- month codes listed, e.g. HMUZ
    roll_offset_days INTEGER NOT NULL,                       -- days before expiry the roll is due
    roll_warn_days   INTEGER NOT NULL,                       -- days before that to start warning
    status           TEXT NOT NULL DEFAULT 'ACTIVE',
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (asset_class IN ('EQUITY','FX','COMMODITY','DEBT','INDEX','CRYPTO','ALTERNATIVE')),
    CHECK (status IN ('ACTIVE','INACTIVE')),
    CHECK (roll_offset_days >= 0 AND roll_warn_days >= 0)
);

COMMENT ON TABLE future_root IS 'Futures roots (ES, NQ, ...): broker mapping, listed months and roll timing.';

INSERT INTO future_root (code, venue, name, asset_class, currency, broker_symbol, broker_exchange, contract_months, roll_offset_days, roll_warn_days) VALUES
    ('ES',  'XCME', 'E-mini S&P 500',       'INDEX', 'USD', 'ES',  'CME', 'HMUZ', 8, 3),
    ('MES', 'XCME', 'Micro E-mini S&P 500', 'INDEX', 'USD', 'MES', 'CME', 'HMUZ', 8, 3),
    ('NQ',  'XCME', 'E-mini Nasdaq-100',    'INDEX', 'USD', 'NQ',  'CME', 'HMUZ', 8, 3)
ON CONFLICT (code) DO NOTHING;

CREATE TABLE future_spread (
    instrument_id BIGINT PRIMARY KEY REFERENCES instrument(id),
    near_id       BIGINT NOT NULL REFERENCES instrument(id),
    far_id        BIGINT NOT NULL REFERENCES instrument(id),
    UNIQUE (near_id, far_id),
    CHECK (near_id <> far_id)
);

COMMENT ON TABLE future_spread IS 'Calendar spread instrument -> its near and far leg instruments.';
