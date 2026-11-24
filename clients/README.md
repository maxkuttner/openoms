# clients

Everything that talks to the OMS over its API, one subdirectory each.

| Directory | What it is |
|-----------|------------|
| [`web/`](web/) | The web app (cockpit): trading at `/app/`, plus the Admin Panel. React + Vite. Built into `web/dist` and embedded in the `oms` binary at compile time (`src/cockpit.rs`), so it ships with every release rather than on its own. |
| [`python/`](python/) | Python client (`oms_client`) for the trading API. Takes a trading token, never the admin surface. |

A new client (another language SDK, a desktop or mobile app, a CLI) gets its own
directory here with its own README. Clients use only the public HTTP API
(`docs/openapi.json`); none of them reaches into the server's code or database.
