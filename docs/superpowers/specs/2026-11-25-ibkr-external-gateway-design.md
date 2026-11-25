# IBKR TWS: external IB Gateway

## Intent

The IBKR TWS connector only works when the OMS runs directly on a host: it starts
IB Gateway through the `docker` CLI and connects to `127.0.0.1:<port>`. Inside a
container (official `compose.yaml`, Dokploy) neither works, so IBKR TWS is
unusable there ("Docker is required for IBKR_TWS but is not running"). Let a
connection point at an IB Gateway that runs as its own service.

## Goals

- Optional connection field **Gateway address** (`host:port`, e.g. `ib-gateway:4004`).
- Set → the OMS never calls Docker; it connects to that address (stream, instrument
  search/import, futures sync, previous closes — every path that uses the live
  client) and reconnects when the gateway restarts. Unreachable → connection down
  with `IB Gateway at <addr> not reachable`, retried like other transient states.
- Set → username and password optional (the login lives in the gateway service).
- Empty → behaviour unchanged (OMS-managed container on 127.0.0.1).
- `compose.yaml`: optional `ib-gateway` service behind profile `ibkr`, same pinned
  image as `gateway::IMAGE`, login + `TRADING_MODE` from `.env`, no published ports.
- Docs: "IBKR in Docker" section on the Interactive Brokers page (compose profile,
  generic container platforms), `.env` keys; venue description mentions futures.

## Non-goals

- The OMS configuring or logging in a remote gateway.
- TLS/auth to the gateway (it is reached over a private Docker network only).
- Dokploy-specific steps in the repo (public repo; ops notes stay out of it).

## Approaches

1. **Gateway address on the connection, gateway as its own service (chosen).**
   Small change; works for compose, Dokploy, k8s.
2. Docker socket + docker CLI + host networking for the OMS container. Rejected:
   gives the OMS control over every container on the host; awkward under Swarm.
3. OMS keeps the login and configures a remote gateway. Rejected: the gateway image
   only takes its login from env at start.

## Design notes for the builder

- Credential: `IbkrTwsCredentials.gateway: Option<String>` (serde default; empty
  string = None); form field "Gateway address" with help text naming the paper
  (4004) / live (4003) ports of the gnzsnz image. Validation: if `gateway` is
  empty, username and password are required (as today); if set, they are optional
  and `gateway` must parse as `host:port` with a valid port.
- `IbapiClient::connect` takes a `host:port` (today `port`); the managed path passes
  `127.0.0.1:<port>`.
- `stream.rs` session loop: with `gateway` set, skip `ensure_running`/log
  classification and connect directly; connect failure → `GatewayState::Unavailable`
  (retried), never `LoginFailed`.
- `on_removed` / container cleanup: no-op for external connections.
- Compose: service `ib-gateway` (`profiles: ["ibkr"]`), env `TWS_USERID`,
  `TWS_PASSWORD`, `TRADING_MODE` (default `paper`) from `.env` with
  `${VAR:-}` defaults so the default profile still starts without them; restart
  unless-stopped. Image string must equal `gateway::IMAGE` — add a test that reads
  compose.yaml and compares.

## Left to the builder

Exact field/struct names beyond the above, error wording details, docs placement
within the IBKR page.
