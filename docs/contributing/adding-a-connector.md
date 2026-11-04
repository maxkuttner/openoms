# Adding a connector

A connector is one broker or venue the OMS can trade through. Its code lives in the gateway crate (`crates/gateway`), one directory per connector, and a new one touches these places only:

1. **`crates/gateway/src/connectors/<name>/`**: `mod.rs` holds the manifest and the `Connector` implementation; next to it go the files the connector needs: `adapter.rs` (REST orders), `stream.rs` (execution stream), `feed.rs` (public quote feed), `fix.rs` (FIX dialect). Copy the closest existing directory:
   - `okx/` or `bybit/` for a crypto venue with a REST API, a private stream and a public quote feed;
   - `alpaca/` for a broker with REST orders and an execution stream;
   - `ibkr/` for a FIX-only broker.

   The manifest holds the venue's entry in the cockpit's connection form, its credential fields, its redaction rules and environments; the `Connector` implementation says how it connects, verifies a key, loads its instrument universe and runs its public feed.
2. **Two lines in `crates/gateway/src/connectors/mod.rs`**: `pub mod <name>;` and its entry in `all()`. The order in `all()` is the order the cockpit lists venues.
3. **A migration** under `db/migrations/ods/oms/`: the venue row, and a `provider_feed_policy` row if its public feed marks instruments (see `0032_INSERT_OKX_FEED_POLICY.sql`).
4. **A docs page** under `docs-site/src/content/docs/integrations/`. The manifest's `docs_url` points at it. What each credential field means and where to get it goes there, not in the UI.

The gateway has no database and no Kafka: a connector reports what the venue did to the order engine over `gateway::contract` and never books anything itself. Code that needs either belongs in the OMS. `boundary_tests::gateway_depends_on_no_database_or_kafka` keeps it that way.

To import a credential from environment variables (`oms config import-env`), add a block to `src/setup/import_env.rs`. This is optional.

`boundary_tests::central_files_name_no_connector` fails if a central file (in the OMS, or one of the gateway's shared modules) starts naming a connector, and `boundary_tests::a_connector_is_one_directory` fails if any of a connector's code is left in the OMS, so this list stays true.
