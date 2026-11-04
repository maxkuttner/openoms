# Adding a connector

A connector is one broker or venue the OMS can trade through. Everything the central code needs to know about it is in its connector module, so a new one touches these places only:

1. **`src/connectors/<name>.rs`**: the manifest and the `Connector` implementation. Copy the closest existing one:
   - `okx.rs` or `bybit.rs` for a crypto venue with a REST API, a private stream and a public quote feed;
   - `alpaca.rs` for a broker with REST orders and an execution stream;
   - `ibkr.rs` for a FIX-only broker.

   The module holds the venue's entry in the cockpit's connection form, its credential fields, its redaction rules and environments, and how it connects, verifies a key, loads its instrument universe and runs its public feed.
2. **One line in `connectors::all()`** (`src/connectors/mod.rs`). The order there is the order the cockpit lists venues.
3. **A migration** under `db/migrations/ods/oms/`: the venue row, and a `provider_feed_policy` row if its public feed marks instruments (see `0032_INSERT_OKX_FEED_POLICY.sql`).
4. **A docs page** under `docs-site/src/content/docs/integrations/`. The manifest's `docs_url` points at it. What each credential field means and where to get it goes there, not in the UI.

The adapter, stream and feed code the connector module calls (`src/adapters/<name>.rs`, `src/<name>_stream.rs`, `src/<name>_feed.rs`) are its own files too. Until those files move into a gateway crate (phase 5b), each also needs its `mod` line in `src/main.rs` or `src/adapters/mod.rs`.

To import a credential from environment variables (`oms config import-env`), add a block to `src/setup/import_env.rs`. This is optional.

`connectors::tests::central_files_name_no_connector` fails if a central file starts naming a connector, so this list stays true.
