-- Calendar spreads tick finer than their legs: CME ES/MES/NQ outrights move in
-- 0.25 index points, their calendar spreads in 0.05. The roll's spread
-- instrument takes its price increment from here, not from the near leg.
ALTER TABLE future_root ADD COLUMN IF NOT EXISTS spread_tick NUMERIC NOT NULL DEFAULT 0.05
    CHECK (spread_tick > 0);

COMMENT ON COLUMN future_root.spread_tick IS 'Calendar spread price increment (index points), e.g. 0.05 for ES.';

UPDATE future_root SET spread_tick = 0.05 WHERE code IN ('ES', 'MES', 'NQ');

-- Spread instruments created before this took the near leg's tick: move them on.
UPDATE instrument i SET price_increment = r.spread_tick, price_precision = scale(r.spread_tick)
FROM future_spread s
JOIN instrument_derivative d ON d.instrument_id = s.near_id
JOIN future_root r ON r.code = d.underlying_symbol
WHERE i.id = s.instrument_id;
