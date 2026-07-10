-- ODS access policy (run against the `ods` database).
--
-- Ownership is established by the migrate runner (CREATE SCHEMA ... AUTHORIZATION):
--   public → mdm_master (master data, "dbo")
--   oms    → oms_user   (operational tables)
-- So oms_user already has full rights on its own `oms` schema. The only
-- cross-schema access needed: the OMS app reads master data in public.

GRANT USAGE ON SCHEMA public TO oms_user;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO oms_user;

-- Instrument seeding + symbology: the OMS app now seeds the master instrument
-- universe itself (`oms setup universe` CLI and the POST /admin/universes/{code}/seed
-- endpoint, src/setup/universe.rs), and its resolver stamps FIGI/CUSIP anchors from
-- OpenFIGI. It fetches definitions from the provider, upserts instrument +
-- instrument_derivative, and writes the universe seed-state (status, last_seeded_at,
-- instrument_count) back on the catalog. So oms_user needs write on those tables.
-- (SELECT on the FK targets venue/currency and the catalog is covered by the
--  blanket public SELECT above; oms.instrument_xref lives in oms_user's own schema.)
GRANT INSERT, UPDATE ON public.instrument, public.instrument_derivative TO oms_user;
GRANT UPDATE ON public.instrument_universe TO oms_user;
-- Editing a universe's underlying/child symbol set (the cockpit checkbox picker)
-- rewrites instrument_universe_symbol.
GRANT INSERT, DELETE ON public.instrument_universe_symbol TO oms_user;

-- Every future master table mdm_master creates is readable by oms_user.
ALTER DEFAULT PRIVILEGES FOR ROLE mdm_master IN SCHEMA public
    GRANT SELECT ON TABLES TO oms_user;

-- Reference-data ingestion: marketbox seeds the instrument universe from
-- Databento (see marketbox/seed_instruments.py), writing the master-data
-- instrument tables and the Databento provider_instrument symbology, and syncs
-- broker symbology (marketbox/broker_sync.py) into broker_instrument. It connects
-- as market_user and needs to *write* instruments + provider_instrument +
-- broker_instrument and *read* the FK targets (venue, currency).
-- (Long-term, a dedicated least-privilege `refdata_user` is cleaner; reusing
--  market_user avoids an infra/ansible role change for now.)
GRANT CONNECT ON DATABASE ods TO market_user;
GRANT USAGE ON SCHEMA public TO market_user;
GRANT SELECT ON public.venue, public.currency TO market_user;
GRANT SELECT, INSERT, UPDATE ON public.instrument, public.instrument_derivative TO market_user;
-- Instrument-universe catalog: the seeder reads enabled universes + their symbols
-- and writes the seed-state columns (status, last_seeded_at, …) back on the parent.
GRANT SELECT ON public.instrument_universe, public.instrument_universe_symbol TO market_user;
GRANT UPDATE ON public.instrument_universe TO market_user;
