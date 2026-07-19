-- Retire the Databento-dataset universe catalog. Instrument seeding is now
-- broker-first (`oms setup sync-broker`): a broker adapter's InstrumentProvider is
-- the source of the master catalog, and feed mapping (`oms setup map-feed`) is the
-- source of feed_instrument. Nothing reads instrument_universe any more (the CLI
-- seeder and the /admin/universes endpoints were removed).

DROP TABLE IF EXISTS instrument_universe_symbol;
DROP TABLE IF EXISTS instrument_universe;
