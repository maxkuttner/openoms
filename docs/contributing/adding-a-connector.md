# Adding a connector

A connector is one broker or venue the OMS can trade through. Its code lives in the gateway crate (`crates/gateway`), one directory per connector, and a new one touches these places:

1. **`crates/gateway/src/connectors/<name>/`**: `mod.rs` holds the manifest and the `Connector` implementation; next to it go the files the connector needs: `adapter.rs` (REST orders), `stream.rs` (execution stream), `feed.rs` (public quote feed), `fix.rs` (FIX dialect). Copy the closest existing directory:
   - `okx/` or `bybit/` for a crypto venue with a REST API, a private stream and a public quote feed;
   - `alpaca/` for a broker with REST orders and an execution stream;
   - `ibkr/` for a FIX-only broker.

   The manifest holds the venue's entry in the cockpit's connection form, its credential fields, its redaction rules and environments; the `Connector` implementation says how it connects, verifies a key, loads its instrument universe and supplies its public feed. A REST connector whose venue has a previous-close endpoint also hands back a `DailyStats` source from `connect`.

   The price side runs under the market data gateway (`crates/gateway/src/md/`): the connector only supplies the feed (its symbology and wire protocol), and `gateway::md` decides what it subscribes, runs the session and emits its quotes. A price-only vendor with no order side, like Databento OPRA, lives in `crates/gateway/src/md/` rather than under `connectors/`.
2. **Two lines in `crates/gateway/src/connectors/mod.rs`**: `pub mod <name>;` and its entry in `all()`. The order in `all()` is the order the cockpit lists venues.
3. **A migration** under `db/migrations/ods/oms/`: the venue row, and a `provider_feed_policy` row if its public feed marks instruments (see `0032_INSERT_OKX_FEED_POLICY.sql`).
4. **A docs page** under `docs-site/src/content/docs/integrations/`. The manifest's `docs_url` points at it. What each credential field means and where to get it goes there, not in the UI.

The gateway has no database and no Kafka: a connector reports what the venue did to the order engine over `gateway::contract` and never books anything itself. Code that needs either belongs in the OMS. `boundary_tests::gateway_depends_on_no_database_or_kafka` keeps it that way.

Beyond those, three tests pin today's list on purpose, so a new connector updates them too:
- `crates/gateway/src/connectors/testdata/venues.json`: the golden `GET /admin/venues` output; add the new venue's entry.
- `connectors::tests::every_public_feed_keeps_its_label`: the feed supervisor names; add the new feed's label, if it has a public feed.
- `feeds::tests::public_feeds_are_the_four_crypto_venues`: the public feed list, likewise.

To import a credential from environment variables (`oms config import-env`), add a block to `src/setup/import_env.rs`. This is optional.

`boundary_tests::only_the_market_data_gateway_sends_quotes` fails if anything outside a feed holds a quote sender, and `boundary_tests::order_adapter_carries_no_prices` if the order adapter serves prices again.

`boundary_tests::central_files_name_no_connector` fails if a central file (in the OMS, or one of the gateway's shared modules) starts naming a connector, and `boundary_tests::a_connector_is_one_directory` fails if any of a connector's code is left in the OMS, so this list stays true.
