# IBKR TWS External Gateway Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an IBKR TWS connection use an IB Gateway that runs as its own service (`host:port`), so IBKR works when the OMS runs in a container.

**Architecture:** An optional credential field `gateway` switches the connector from OMS-managed Docker to a direct TCP connect; username/password become optional in that mode. `compose.yaml` gets an opt-in `ib-gateway` service.

**Tech Stack:** Rust (gateway crate, axum), ibapi 5.7.0, React cockpit, docker compose.

**Spec:** `docs/superpowers/specs/2026-11-25-ibkr-external-gateway-design.md`

## Global Constraints

- Empty `gateway` → behaviour unchanged (managed container on 127.0.0.1).
- Set `gateway` → no Docker call anywhere on that connection's path.
- Unreachable gateway → `IB Gateway at <addr> not reachable: <err>`, retried (never a stop state).
- Paper port 4004, live port 4003 (gnzsnz image socat ports, `gateway::CONTAINER_PORT_PAPER/LIVE`).
- compose `ib-gateway` image string == `gateway::IMAGE`; profile `ibkr`; no published ports.
- Public repo: no server names/hostnames.
- Build: `OPENSSL_ROOT_DIR=/opt/homebrew/opt/openssl@3`.

## Review Focus

1. Editing a configured external connection and clearing the address must switch back to managed mode (non-secret optional field: submitted empty clears). → Task 1 test.
2. Managed connection without username/password is refused at save with a clear message. → Task 2 test.
3. `gateway` with no port / bad port / spaces (`ib-gateway`, `ib-gateway:99999`, ` ib-gateway:4004 `) → refused at save; surrounding whitespace trimmed. → Task 2 test.
4. External mode never classifies gateway logs as `LoginFailed` (that path stops the stream). → Task 3 test.
5. Default `docker compose up` (no profile) still starts without `TWS_*` in `.env`. → Task 4 test.

---

### Task 1: Optional credential fields

**Files:** `crates/gateway/src/venue.rs` (CredentialField), `src/credentials_api.rs` (`parse_broker` L134), `clients/web/src/components/CredentialsPanel.tsx` (validate L113), `clients/web/src/venues.ts` (Venue field type)

**Interfaces — Produces:** `CredentialField.optional: bool` (serialized), builder `fn optional(self) -> Self`. Merge rule for optional fields in `parse_broker`: omitted → stored value or `""`; submitted (even empty) for a **non-secret** optional field → that value (trimmed); secret optional field keeps today's rule (empty keeps stored, nothing stored → `""`).

- [ ] Tests in `src/credentials_api.rs` tests: `optional_field_may_be_missing` (no stored, omitted → `""`), `optional_text_field_submitted_empty_clears` (stored `"ib-gateway:4004"`, submitted `""` → `""`), `optional_secret_submitted_empty_keeps` (stored `"p"`, submitted `""` → `"p"`); required fields still `MissingField`. Web: `venues.test.ts`/new helper test that the validator skips `optional` fields.
- [ ] RED, implement, GREEN (`cargo test credentials_api`, `npm --prefix clients/web test`).
- [ ] Form shows optional fields without the required asterisk.
- [ ] Commit `feat(credentials): optional credential fields`.

### Task 2: IBKR TWS gateway field + validation

**Files:** `crates/gateway/src/connectors/ibkr_tws/mod.rs` (credentials L50, `venue()` L150, `connect` L101)

**Interfaces — Produces:** `IbkrTwsCredentials.gateway: Option<String>` (serde default; empty/whitespace → None); `impl IbkrTwsCredentials { pub fn external(&self) -> Result<Option<(String, u16)>, String>; pub fn check(&self) -> Result<(), String> }`. `check`: gateway set → must parse `host:port` (non-empty host, port 1..=65535) after trim; gateway unset → username and password non-empty, else `"username and password are required unless a gateway address is set"`. Venue fields: `username`, `password` (secret) and new `gateway` all `.optional()`; `gateway` label `"Gateway address (optional): host:port of an IB Gateway you run yourself, e.g. ib-gateway:4004 (paper) or :4003 (live). Leave empty to let the OMS start it with Docker."`. Venue description → `"Stocks, futures and options via IB Gateway"`. Save-time: `parse_broker` must reject a credential whose `check` fails — add `Manifest.validate: fn(&BrokerCredentials) -> Result<(), String>` (default `|_| Ok(())` for other connectors) called at the end of `parse_broker`, mapped to `CredentialError::BadField { name: "gateway" or "username", why }`. `connect` also calls `check` (defence).

- [ ] Tests: `gateway_address_parses` (`"ib-gateway:4004"`→`("ib-gateway",4004)`, `" ib-gateway:4004 "` trimmed, `""`→None, `"ib-gateway"`/`"ib-gateway:99999"`/`":4004"` → Err); `managed_needs_login`; `external_needs_no_login`; `parse_broker` refuses bad gateway and managed-without-login.
- [ ] RED, implement, GREEN (`cargo test -p gateway ibkr_tws`, `cargo test credentials`), existing venue tests stay green.
- [ ] Commit `feat(ibkr_tws): gateway address field`.

### Task 3: Connect to an external gateway

**Files:** `crates/gateway/src/connectors/ibkr_tws/client.rs` (`connect` L289), `stream.rs` (`supervise` L281), `mod.rs` (`connect`)

**Interfaces — Consumes:** `IbkrTwsCredentials::external()`. **Produces:** `IbapiClient::connect(addr: &str, client_id: i32)`; managed path passes `format!("127.0.0.1:{port}")`. In `supervise`: when `gw.creds.external()` is `Some((host, port))`, skip `ensure_running` and log classification; connect `host:port`; on error `Next::Wait(format!("IB Gateway at {host}:{port} not reachable: {e}"))`. Extract the per-iteration decision into a testable fn (e.g. `async fn attempt(gw: &Gateway, connect: impl …) -> Next`) or inject the connect fn — builder's choice — so tests need no network.

- [ ] Tests: `external_gateway_skips_docker` (a `GatewayDocker` fake that panics/records on any call; external creds → no calls); `unreachable_external_gateway_waits_not_stops` (connect error → `Next::Wait` containing the address); managed path unchanged (existing tests green).
- [ ] RED, implement, GREEN (`cargo test -p gateway`).
- [ ] Commit `feat(ibkr_tws): connect to an external IB Gateway`.

### Task 4: Compose profile + docs

**Files:** `compose.yaml`, `.env.example`, `docs-site/src/content/docs/integrations/interactive-brokers.md`, docs-site installation/config pages if they list `.env` keys; test in `crates/gateway/src/connectors/ibkr_tws/gateway.rs` tests (or wherever repo-file tests live).

**Produces:** service
```yaml
  ib-gateway:
    image: <exactly gateway::IMAGE>
    profiles: ["ibkr"]
    environment:
      TWS_USERID: ${TWS_USERID:-}
      TWS_PASSWORD: ${TWS_PASSWORD:-}
      TRADING_MODE: ${TRADING_MODE:-paper}
    restart: unless-stopped
```
(check the image's README/env names used in `gateway.rs` `write_env_file` and mirror them, incl. any 2FA/session settings it sets). Docs: "IBKR in Docker" — compose `--profile ibkr`, `.env` keys, connection field `ib-gateway:4004`/`:4003`, 2FA push on first login, daily IBKR restart/reconnect; generic note for other container platforms (run the image as a sibling service on a private network, set the address).

- [ ] Test `compose_ib_gateway_image_matches` reads `../../compose.yaml` (via `env!("CARGO_MANIFEST_DIR")`) and asserts it contains `gateway::IMAGE` and `profiles: ["ibkr"]`.
- [ ] `docker compose -f compose.yaml config` succeeds with an `.env` lacking `TWS_*` (only POSTGRES_PASSWORD set) — run it, paste output summary in the report.
- [ ] Commit `feat(compose): optional ib-gateway service; docs`.
