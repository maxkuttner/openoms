-- ODS access policy (run against the `ods` database).
--
-- Ownership is established by the migrate runner (CREATE SCHEMA ... AUTHORIZATION):
--   public → mdm_master (master data, "dbo")
--   oms    → oms_user   (operational tables)
-- So oms_user already has full rights on its own `oms` schema. The only
-- cross-schema access needed: the OMS app reads master data in public.

GRANT USAGE ON SCHEMA public TO oms_user;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO oms_user;

-- Instrument seeding + symbology: the OMS app seeds the master instrument catalog
-- itself, broker-first. Broker sync (`oms setup sync-broker`) creates the master
-- public.instrument (+ instrument_derivative) rows and the broker_instrument mapping
-- in one pass. Feed mapping is derived at runtime, not stored; the
-- resolver stamps FIGI/CUSIP anchors from OpenFIGI. So oms_user needs write on the
-- master catalog and both mapping tables. (SELECT on the FK targets venue/currency
-- is covered by the blanket public SELECT above.)
GRANT INSERT, UPDATE ON public.instrument, public.instrument_derivative TO oms_user;
GRANT INSERT, UPDATE, DELETE ON public.broker_instrument TO oms_user;

-- Every future master table mdm_master creates is readable by oms_user.
ALTER DEFAULT PRIVILEGES FOR ROLE mdm_master IN SCHEMA public
    GRANT SELECT ON TABLES TO oms_user;

-- Reference-data ingestion runs in-process as oms_user (`oms setup universe` /
-- `oms setup sync-brokers`), using the grants above — no separate role needed.
--
-- market_user previously held write grants here for the marketbox seeders
-- (seed_instruments.py / broker_sync.py), both now gone. db-provision never creates
-- this role, so a fresh install skips the block entirely; it exists only to converge
-- databases provisioned back when the seeders were external. Revoking (not merely
-- deleting the GRANTs) is what actually removes the stale access. Dropping the role
-- is a one-off admin action — it is cluster-wide and may serve other databases, so
-- it is deliberately not done here.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'market_user') THEN
        REVOKE ALL ON public.instrument, public.instrument_derivative FROM market_user;
        REVOKE ALL ON public.venue, public.currency FROM market_user;
        REVOKE USAGE ON SCHEMA public FROM market_user;
        EXECUTE format('REVOKE CONNECT ON DATABASE %I FROM market_user', current_database());
    END IF;
END $$;
