-- ODS access policy (run against the `ods` database).
--
-- There is one role, `oms`, and it owns everything: the database, both schemas,
-- and every table the migrations create (`CREATE SCHEMA ... AUTHORIZATION oms`,
-- then `SET ROLE oms` per migration). Ownership already implies full rights, so
-- this file exists to cover the objects ownership does not reach — anything the
-- superuser created directly — and to make the intent explicit rather than
-- implied.
--
-- The schema split is kept for consumers, not for permissions: `public` holds
-- master data (instruments, venues, currencies, calendars) that other services
-- can read, `oms` holds this application's operational tables.

GRANT ALL PRIVILEGES ON SCHEMA public, oms TO oms;
GRANT ALL PRIVILEGES ON ALL TABLES    IN SCHEMA public, oms TO oms;
GRANT ALL PRIVILEGES ON ALL SEQUENCES IN SCHEMA public, oms TO oms;
