-- Cluster-wide role attributes (idempotent).
--
-- The roles themselves are created by `make db-provision` (db/scripts/provision.sh)
-- — this project needs nothing but a running Postgres. search_path is a per-role
-- default that applies across all databases the role connects to.

ALTER ROLE mdm_master SET search_path TO public;        -- master data lives in public ("dbo")
ALTER ROLE oms_user   SET search_path TO oms, public;   -- own tables in oms; read master in public
