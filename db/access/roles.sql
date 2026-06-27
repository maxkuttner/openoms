-- Cluster-wide role attributes (idempotent).
--
-- The roles themselves (with passwords) are created by infra/ansible from the
-- vault. mdm only manages their *attributes* and grants. search_path is a
-- per-role default that applies across all databases the role connects to.

ALTER ROLE mdm_master  SET search_path TO public;        -- master data lives in public ("dbo")
ALTER ROLE oms_user    SET search_path TO oms, public;   -- own tables in oms; read master in public
ALTER ROLE market_user SET search_path TO public;        -- market data in mds.public
