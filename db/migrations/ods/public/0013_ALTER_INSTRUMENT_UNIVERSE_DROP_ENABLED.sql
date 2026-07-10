-- Retire the `enabled` opt-in flag. Seeding is now driven per-universe from the
-- cockpit (click to seed a universe), not a bulk "seed every enabled one" run,
-- so the flag and its index no longer serve a purpose.

DROP INDEX IF EXISTS idx_instrument_universe_enabled;
ALTER TABLE instrument_universe DROP COLUMN IF EXISTS enabled;
