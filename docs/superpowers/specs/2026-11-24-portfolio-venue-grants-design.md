# Trading grants and portfolio groups

## Intent

Creating a portfolio and trading it should need no "account" step, and the admin
should control **what** a portfolio may trade through grants. Venues are the first
grant; instrument classes on a venue and time-of-day windows are expected next, and
later grants may attach to groups or users, not only portfolios. Adding a grant kind
must be cheap: no migration, one rule, one small form.

## Goals

- **Owner group.** When an admin creates a portfolio, a group `portfolio <code>` is
  created with it: the creating admin as sole member, access **view**. An ordinary
  group (editable, deletable). Only admins create portfolios.
- **Generic grants.** One table and one Rust type hold every grant kind. A grant has
  a subject (today always a portfolio), a kind and typed parameters.
- **Venue grant** (built now): which broker connections a portfolio may trade on.
- **Pre-trade check** runs every applicable grant kind on each order.
- The word "account" disappears from the UI and docs.

## Non-goals

- Building instrument-class or hours grants now (the design shows their shape only).
- Grants on groups or users now (the schema allows it; nothing uses it).
- Removing the `account` table (deferred).

## Approaches

1. **One grants table, typed rule per kind (chosen).** `trading_grant` with `kind`
   and `params jsonb`, a Rust `enum Grant` validated with serde, one check function.
   New kind = enum variant + check + form, no migration.
2. Table per kind. Strict DB types, but every kind needs its own migration, API and
   UI: the opposite of easy extension.

Venues stay on the portfolio (decided), not on group access.

## Design

- **Table** `trading_grant(id uuid pk, subject_type text CHECK IN ('portfolio'),
  subject_id uuid, kind text, params jsonb, status text CHECK IN ('ACTIVE','REVOKED'),
  created_at, updated_at)`. Unique per `(subject_type, subject_id, kind, params)`.
  Revoking sets `REVOKED` (history kept); re-granting reactivates.
- **Type** `enum Grant { Venue { connection: String, account_ref: Option<String> } }`,
  `#[serde(tag = "kind", content = "params")]`. Documented future shapes:
  `InstrumentClass { connection, class }`, `Hours { tz, days, from, to }`.
- **Default per kind**, declared by the kind (`fn when_absent() -> Absent`):
  - `Venue`: **deny** — no venue grant, nothing tradeable.
  - every other kind: **allow** — no grant of that kind, no restriction.
  With grants of a kind present, the order must match at least one of them.
- **Check** `grants::check(&OrderFacts, &[Grant]) -> Result<(), Denied>` with
  `OrderFacts { connection, broker_code, instrument_class, at: DateTime<Utc> }`. Runs
  in order entry before the risk engine; `Denied` carries a message such as
  `venue not granted to this portfolio`.
- **Routing.** Granting a venue also upserts the hidden `account` row
  `(portfolio_id, connection)` (code `<portfolio>-<connection>`, ref `""`, or the IBKR
  account, which the admin must supply for IBKR). Revoking closes that row. Orders,
  fills, recon and strategies keep using `account_id`. The old unique
  `(connection, external_account_ref)` becomes unique `(portfolio_id, connection)`.
  A venue used by a strategy cannot be revoked.
- **Shared venue accounts.** Two portfolios on one connection share the venue's
  account: external fills resolve to one deterministic row and recon pulls each
  venue account once.
- **API** `GET/PUT /admin/portfolios/:id/grants` (the full active set, any kind),
  audited; MFA as all admin writes. The ticket's `/portfolios/:id/venues` lists only
  granted venues; submit picks the single granted venue or answers 422
  `choose a venue`.
- **UI.** Portfolio admin screen gets **Grants**; today one section, **Venues**
  (checklist; IBKR asks for its account). Each future kind adds a section.

## Left to the builder

- Where `instrument_class` comes from for `OrderFacts` (instrument catalog column);
  only needed once that kind exists, so `OrderFacts` may carry `None` now.
- Exact admin layout of the Grants dialog.
