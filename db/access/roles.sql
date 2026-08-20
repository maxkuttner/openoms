-- Cluster-wide role attributes (idempotent).
--
-- The role itself is created by `oms database init`. search_path is a per-role
-- default that applies across every database the role connects to.

ALTER ROLE oms SET search_path TO oms, public;
