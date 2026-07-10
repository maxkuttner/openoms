-- Allow a PARTIAL seed status: an OPTION universe where some chosen underlyings
-- got their option chains but others didn't (e.g. the underlying equity isn't
-- seeded, or the provider returned no chain).

ALTER TABLE instrument_universe DROP CONSTRAINT IF EXISTS instrument_universe_status_check;
ALTER TABLE instrument_universe ADD CONSTRAINT instrument_universe_status_check
    CHECK (status IN ('PENDING', 'SEEDING', 'SEEDED', 'PARTIAL', 'ERROR'));
