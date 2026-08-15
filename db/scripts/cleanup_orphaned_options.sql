-- One-off cleanup: remove option rows left behind by the retired Databento-dataset
-- seeding model.
--
-- Before broker-first seeding (migration 0018), `scripts/seed_instruments.py` built
-- the catalog from Databento definition files. Those rows carry Databento's strict
-- 21-char OSI symbol (root space-padded to 6) and — because that model had no notion
-- of one — no `broker_instrument` routing handle at all. They cannot be ordered and
-- cannot be closed, but they still match the OPRA feed's `candidates()` filter, so
-- the quote feed keeps subscribing to contracts the OMS could never trade.
--
-- Broker-first replaced this: an adapter's InstrumentProvider writes the master row
-- and the routing mapping in one pass (src/setup/brokers.rs), so "no broker mapping"
-- is now precisely the signature of a row from the old model. A catalog seeded fresh
-- has none of these, which is why this is a maintenance script rather than a
-- migration — on a clean database it is a no-op.
--
-- Not a migration for a second reason: the guard below must read `oms.position` and
-- `oms.order_state`, and the migration runner executes public-schema files as
-- mdm_master, which has no access to the oms schema. Run this as the cluster admin
-- (ADMIN_USER), which can see both.
--
-- Rows referenced by a position or an order are kept, not deleted. History must stay
-- resolvable — an order that names an instrument id has to be able to resolve it, and
-- a stranded position is something preflight reports rather than something a cleanup
-- silently erases. Those survivors keep whatever status they have; the expiry sweep
-- (src/expiry.rs) will already have marked the dated ones EXPIRED.
--
-- instrument_derivative rows go with them via ON DELETE CASCADE.
--
-- Idempotent: re-running deletes nothing once the orphans are gone.
--
--   psql "$ADMIN_URL" -f db/scripts/cleanup_orphaned_options.sql

\set ON_ERROR_STOP on

BEGIN;

CREATE TEMP TABLE orphaned_options ON COMMIT DROP AS
SELECT i.id, i.symbol
FROM public.instrument i
LEFT JOIN public.broker_instrument bi ON bi.instrument_id = i.id
WHERE i.venue = 'OPRA'
  AND i.instrument_class = 'OPTION'
  AND bi.instrument_id IS NULL
  -- Keep anything the operational schema still points at.
  AND NOT EXISTS (SELECT 1 FROM oms.position    p WHERE p.instrument_id = i.id::text)
  AND NOT EXISTS (SELECT 1 FROM oms.order_state s WHERE s.instrument_id = i.id::text)
  AND NOT EXISTS (SELECT 1 FROM oms.allocation  a WHERE a.instrument_id = i.id::text)
  AND NOT EXISTS (SELECT 1 FROM oms.risk_limits r WHERE r.instrument_id = i.id::text)
  -- And anything still serving as another instrument's underlying.
  AND NOT EXISTS (SELECT 1 FROM public.instrument_derivative d WHERE d.underlying_id = i.id);

SELECT count(*) AS to_delete FROM orphaned_options;

DELETE FROM public.instrument i
USING orphaned_options o
WHERE i.id = o.id;

-- What survived, and why it was kept.
SELECT i.symbol, i.status
FROM public.instrument i
LEFT JOIN public.broker_instrument bi ON bi.instrument_id = i.id
WHERE i.venue = 'OPRA' AND i.instrument_class = 'OPTION' AND bi.instrument_id IS NULL
ORDER BY 1;

COMMIT;
