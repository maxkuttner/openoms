# Reported strategies (path 2, simplified) — design

Source: `STRATEGY2.md` (2026-11-22). Server side only; the Python SDK is a separate follow-up.

## Intent

A path 2 strategy runs anywhere, in any language, against any venues with the trader's
own keys. The OMS issues a per-strategy reporting token, receives fill batches, and books
them into the strategy's one portfolio. Today's co-located contract (dedicated account,
two keys, deployment, epoch, journal, control link) blocks non-Rust, cross-venue and
no-sub-account strategies. Path 2 only needs identity and accounting.

## Goals

- `REPORTED` strategy mode: owner, one portfolio, reporting token. No account, connection,
  deployment, FIX ids or control link.
- `POST /api/v1/strategies/{code}/fills`: batch ingest, accepted per fill.
- Idempotent at batch (`batch_id`) and fill (`venue + account_ref + exec_id`) level.
- Fills placed at `trade_time`; late fills rebuild the position in trade-time order.
- Positions aggregate by instrument across venues; venue/account_ref kept on the fill.
- Unbookable fills become breaks (`source = REPORTED`), adoptable and dismissable in the
  existing Breaks UI, labelled "reported, not verified".
- Visibility: reported fills list, last report time, sequence gaps, rejected batches,
  stale-strategy warning.

## Non-goals

- Python SDK (spool, replay, batching): follow-up project.
- Position snapshots, venue verification (read key / statement import): v2.
- Sub-books inside a strategy. Separate P&L = separate strategy + book.
- Any change to existing VIA_OMS or COLOCATED strategies; removing colo runtime/control code.

## Anti-goals

- A bad fill rejecting a whole batch.
- Reported fills showing up as OMS orders (they are not orders; no open-order state).
- Reported data presented as verified anywhere in the UI.
- Fake accounts/connections created to satisfy existing foreign keys.

## Approaches considered (for the fill ledger)

1. Synthetic broker_connection + account per strategy, reuse `venue_fill`/`order_state`.
   Maximum reuse, but one fake account per `account_ref`, fake rows leak into admin lists
   and recon loops, and quietly reintroduces account registration.
2. Make `account_id`/`broker_connection_code` nullable on `venue_fill`, `recon_break`,
   `order_state`. Touches the core order table and every query assuming them. Rejected.
3. **Dedicated `reported_fill` ledger** — picked. Dedupe key matches the spec, row keeps
   the raw payload, books directly into positions. Trade-off: reported fills need their
   own view instead of the orders blotter, which matches "OMS keeps no open orders".

## Design

### Schema (new migration, `0068_REPORTED_STRATEGIES.sql`)

- `strategy`: allow `mode = 'REPORTED'`; relax `principal_id`, `sender_comp_id`,
  `target_comp_id` to nullable with a CHECK that they are set for non-REPORTED modes.
- `strategy_reporting_token(id, strategy_code, token_hash, created_at, revoked_at,
  expires_at)`: hash only, shown once. One active token plus at most one in a rotation
  grace window (`expires_at` on the old one, default 24h).
- `reported_batch(strategy_code, batch_id, sequence, received_at, fill_count, booked,
  broken, duplicate, payload_json)`, PK `(strategy_code, batch_id)`. Audit + batch idempotency.
- `reported_fill(id, strategy_code, batch_id, venue, account_ref, exec_id,
  client_order_id, symbol, instrument_id, side, qty, price, fee, fee_asset, trade_time,
  booked_at, raw_json)`, UNIQUE `(strategy_code, venue, account_ref, exec_id)`.
- `recon_break`: add `source TEXT NOT NULL DEFAULT 'RECONCILED'` (`RECONCILED|REPORTED`),
  `strategy_code` nullable FK; make `broker_connection_code`, `account_id`,
  `venue_order_id` nullable with a CHECK by source. Add `reported_fill_json`. Unique open
  index for REPORTED breaks on `(strategy_code, venue, account_ref, exec_id)` so retries
  don't duplicate.

### Strategy creation (trader-owned)

Overrides STRATEGY2.md's "books are never auto-created" for this mode. Creating a
REPORTED strategy, in one transaction:

1. Creates a portfolio (`code` derived from the strategy code, e.g. `RPT-<code>`).
2. Creates a group for it with the trader as the only member and `manage` access to that
   portfolio. Groups are the only way a person gets portfolio access (0065), so the
   strategy's portfolio follows the same rule instead of a direct grant.
3. Creates the strategy (`owner_principal_id` = trader, `mode = REPORTED`) and its first token.

At creation only the trader and admins see the portfolio. The group is marked as the
strategy's group (e.g. `user_group.strategy_code`) and shown as such. The trader can't
change its membership; admins can add or remove people through the existing admin
groups UI, each with the usual access levels. The owner can't be removed from it.
Trader self-service for adding people is a possible later step, not v1.

### Ingest

1. Bearer token → strategy (must be REPORTED, ACTIVE). Path `{code}` must match the token's
   strategy, else 403. Any portfolio/book field in the payload is ignored.
2. Per-strategy rate limit (token bucket in memory; limits returned by the config call).
   Max batch size enforced → 413.
3. Seen `batch_id` → return the stored result, book nothing.
4. Per fill, in its own savepoint: validate shape; resolve instrument via
   `broker_instrument (broker_code = venue, broker_symbol = symbol)`; insert into
   `reported_fill` `ON CONFLICT DO NOTHING` (conflict = duplicate; differing payload =
   break kind `CONFLICTING_DUPLICATE`); call `positions::persist_fill` with the strategy's
   portfolio, `trade_time` and `exec_id`; apply fee.
5. Unresolvable instrument/venue or invalid fill → `recon_break` (source REPORTED, raw fill
   stored). Response lists per-fill outcome: `booked | duplicate | break`.
6. Record the batch; sequence gap vs the last sequence → flag on the batch + alarm only.

### Positions

- `replay_sql` gets a branch reading `reported_fill JOIN strategy` (portfolio from the
  strategy) so rebuilds, late fills and `rebuild_positions` include reported fills.
  Canonical order key `(trade_time, exec_id)`; `global_position` NULL for these rows.
- Fees: fee asset = instrument quote currency → `FeeEffect::Quote`; = base → `Base`;
  anything else (e.g. BNB) → `Other` (recorded on the fill, no position effect, alarm).
  `persist_fee` needs a variant not tied to `order_id` (or the order id made optional).

### Breaks

- Adopt a REPORTED break: operator picks instrument (and book; defaults to the strategy's
  portfolio, may override as an explicit rebook). Books exactly the stored fill through the
  same path as ingest, once. Adding the `broker_instrument` mapping makes later fills book.
- Dismiss: drops it; the `reported_fill` row is not created.

### API / UI

- `GET /api/v1/strategies/{code}/config` (token): strategy code, portfolio, schema version,
  batch/rate limits.
- Trader self-service (not `/admin`): any logged-in trader creates a REPORTED strategy,
  mints/rotates/revokes its token (shown once), and sees only their own. They manage it
  and are responsible for it. Admins can see and act on all of them (revoke, adopt breaks).
- Cockpit strategy page for REPORTED mode: reported fills, last report, sequence gaps,
  rejected batches, stale warning (no report for N minutes, configurable per strategy),
  "self-managed / unverified" label. Breaks page: source column + filter.
- Request carries `schema_version`; unknown version → 400.

## Decisions (open items in STRATEGY2.md)

- Traders create and own their reported strategies; a portfolio + private group is
  auto-created per strategy; admins can add other people to it (Max, 2026-11-23).

- 2 Sequence gaps: alarm only.  3 Position snapshot: v2.  4 Venue verification: v2.
- 5 Tokens: one active, rotation with 24h grace on the previous one.
- 6 Unknown fee asset: book the fill, fee `Other`, alarm (not a break).
- 7 `/api/v1/strategies/{code}/fills` + `schema_version: 1`.
- 8 Dedicated `reported_fill` ledger (approach 3).

## Left to the builder

- Exact rate-limit defaults, stale threshold default, response JSON field names.
- Whether `persist_fee` gets a new function or an optional order id.
- Check that admins see every portfolio without a group (expected; confirm in `authz.rs`).
- Migration of the CHECK constraints on `strategy` without disturbing existing rows.

## Acceptance

As in STRATEGY2.md, server-side, exercised with HTTP/integration tests instead of the
Python SDK: report → position/P&L correct; replay books once; late fill at trade time;
cross-venue aggregation keeps venue; unknown instrument → break → adopt books once →
later fills book; no credentials/accounts/deployment needed; revoked token → 401.
