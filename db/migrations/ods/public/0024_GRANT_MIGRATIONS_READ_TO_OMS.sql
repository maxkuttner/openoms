-- `oms database status` reports which migrations are applied. That is a read-only
-- inspection and must not require the superuser credential — the one that can drop
-- the database. pg_roles and pg_database are public catalogs; only the tracking
-- table needed a grant.
--
-- The table is owned by the connecting admin (see migrate.rs: the tracking row is
-- written after RESET ROLE), so this grant is what lets the ordinary role read it.
--
-- Every migration file runs under `SET ROLE oms` (apply_one in migrate.rs), but
-- GRANT on an object requires being its owner or the superuser — `oms` is neither
-- for this table. RESET ROLE drops back to the connecting admin for this one
-- statement; apply_one's own RESET ROLE right after is then a harmless no-op.
--
-- Caveat: RESET ROLE returns to the *session* (connection) user, not to a
-- superuser as such — this GRANT only succeeds if whoever `database init`/
-- `migrate` connected as is a superuser or the table's owner. That is true for
-- every path this project ships (the connecting role is always the superuser
-- given to `init`/`migrate`), so it is noted here rather than enforced in code.

RESET ROLE;
GRANT SELECT ON public._mdm_migrations TO oms;
