# Venue selection on the order ticket — design

## Context

The trader wants to choose which broker/venue an order routes through at
submission time — e.g. "buy BTCUSDT via Binance specifically" — instead of
the system silently using whatever the portfolio's default account happens
to be. Investigating the current routing mechanics turned up three facts
that shape this design:

1. **Routing is keyed off `account`, not instrument or venue.**
   `orders_submit` (`src/handlers.rs`) resolves an `account_id` → its
   `broker_connection` → `broker_code`/`environment`, and *that* determines
   where the order routes. `account_id` is already an optional field on the
   submit request — it's just never populated by the trade webapp today,
   which always omits it and falls through to `portfolio.default_account_id`.
2. **A portfolio can only ever have one account today.** `portfolio` has a
   single `default_account_id` FK; there is no relationship letting a
   portfolio have more than one account to choose between. Structurally,
   there is nothing to pick from yet, independent of any UI work.
3. **`orders_submit` never validates that a supplied `account_id` belongs to
   the given `portfolio_id` at all.** It only checks the account exists and
   its broker connection is `ACTIVE`. This is a real, pre-existing gap this
   design closes as a side effect, not its main goal.

Checked live against this deployment's actual data: only two broker
connections exist (`alpaca-paper`/ALPACA, `binance-paper`/BINANCE). Bybit is
currently a market-data feed only — there is no Bybit `BrokerAdapter`, so it
has no `broker_instrument` catalog rows and cannot appear as a venue option
under any design. Building Bybit (or any other new broker) as a tradeable
adapter is explicitly a separate, later undertaking.

## Goals

- The order ticket shows every broker that has ever synced a catalog row
  for the selected instrument (`broker_instrument`), each marked usable or
  not *for the currently selected portfolio*, with a reason when not.
- Picking a venue routes the order through that specific account; not
  picking one preserves today's exact behavior (silent default-account
  fallback).
- Close the account-ownership validation gap: an explicit `account_id` must
  belong to the order's `portfolio_id`, or the submission is rejected.

## Non-goals

- Building any new broker adapter (Bybit or otherwise) as a tradeable
  venue — a real broker integration is its own project.
- Smart order routing / automatic best-venue selection, price comparison
  across venues, or anything resembling execution quality analysis — already
  ruled out as a non-goal earlier in this app's design (no backing data
  model for it).
- A static "known venues per asset class" registry independent of whether a
  broker has actually been integrated here. The venue list is exactly
  `broker_instrument`'s existing rows for the instrument — nothing invented
  ahead of real integration.
- Changing how `default_account_id` itself is chosen or edited — it remains
  a portfolio's preselected account; this design only adds *other* accounts
  a portfolio may additionally hold and choose between.

## Architecture

**Schema.** `account` gains `portfolio_id UUID REFERENCES portfolio(id)`
(nullable — an account not yet assigned to a portfolio simply can't be
picked by anyone, same as today's implicit state for every account other
than a portfolio's default). Backfilled from the existing
`portfolio.default_account_id` relationship: every account that is
currently *someone's* default gets that portfolio's id; accounts that are
nobody's default stay `NULL`. `default_account_id` itself is untouched — it
keeps meaning "preselect this one," now among potentially several a
portfolio has.

**New endpoint**: `GET /portfolios/:id/venues?instrument_id=X`. Gated by the
same `can_view` grant check `get_portfolio_positions` already uses. Query
plan: start from every `broker_instrument` row for `instrument_id` (this is
deliberately the full catalog, not filtered to "currently usable" — an
inactive connection or missing account still needs to appear, grayed out,
with its reason), left-join `broker_connection` on `broker_code`, left-join
`account` on `broker_connection_code = broker_connection.code AND
account.portfolio_id = :id`. For each row, compute:
- `eligible = broker_instrument.is_tradeable AND broker_connection.status =
  'ACTIVE' AND account.id IS NOT NULL`
- `reason`, when not eligible, is exactly one of: `"not tradeable on this
  broker"` (catalog says so), `"broker connection is not active"`, or
  `"no account on this connection for this portfolio"` — checked in that
  order, so the response always names the single most fundamental blocker
  rather than layering all applicable ones.

**`orders_submit` change.** When `req.account_id` is `Some(_)`, add one more
check alongside the existing "account exists, connection active" query:
that account's `portfolio_id` must equal the request's `portfolio_id`, or
the whole submission is rejected before anything is written — same
BAD_REQUEST-class failure as the existing UUID-parse checks just above it,
not a 500. When `req.account_id` is `None`, behavior is byte-for-byte
unchanged (falls through to `default_account_id`).

**Admin UI.** `cockpit/src/pages/Accounts.tsx`'s `CrudResource` gains a
`portfolio_id` select field (`optionsPath: "/admin/portfolios"`) so an
operator can actually assign a second account to a portfolio to exercise
this feature — otherwise there is no way to create the multi-account state
this design exists to expose. `list_accounts`/`Account`
(`src/domain/identity.rs`) gain the `portfolio_id` field end to end
(struct, `CreateAccount`/`UpdateAccount`, the SQL in `admin.rs`) the same
way every other column already flows through that file.

**Frontend.** `OrderTicket.tsx` gets a new venue `Select`, populated once
both `portfolioId` and `instrumentId` are set (`enabled: !!portfolioId &&
!!instrumentId` on its own `useQuery`, refetched on either changing). Every
returned row renders as an option; ineligible ones are `disabled` with the
`reason` as their `title` (a native tooltip — no new dependency). The
`Select` is always shown once the query returns ≥1 row, even if there's
only one option, so it's always visible which venue an order is routing
through. Picking a venue sets a new piece of ticket state
(`selectedAccountId`); submitting includes `account_id: selectedAccountId`
only when it's been explicitly set, otherwise the field is omitted exactly
as it is today. The confirmation modal's existing summary line gains the
chosen venue's `broker_code` when one was picked.

## Data flow

1. Trader picks a portfolio and an instrument (unchanged).
2. Ticket fires `GET /portfolios/{portfolioId}/venues?instrument_id={instrumentId}`.
3. Dropdown renders every row; trader either leaves it unset (today's
   behavior) or picks an eligible one.
4. `POST /orders/submit` includes `account_id` only if one was picked.
5. `orders_submit` validates (new) that a given `account_id` belongs to
   `portfolio_id`, resolves routing from it exactly as today, and proceeds
   unchanged from that point on.

## Error handling

- Unknown/uncataloged `instrument_id` on the venues endpoint → empty list,
  not an error — same "excluded, not fabricated" convention used
  everywhere else in this app (e.g. `Positions.tsx`'s unpriced-position
  handling).
- No `can_view` grant on the portfolio → `403`, identical to the existing
  positions endpoint's own check.
- `account_id` supplied but not owned by `portfolio_id` on submit → `400`
  with a message naming the mismatch, checked *before* any event-store
  write (same ordering as the existing UUID-parse validations at the top of
  `orders_submit`) — never a 500, never a partially-committed order.
- Zero eligible venues (the common case today, since most instruments only
  have one broker in their catalog) → ticket behaves exactly as it does
  now: the dropdown shows the one option informationally, or is empty and
  submission falls through to the default account unchanged.

## Testing

- Backend, DB-backed (properly `#[ignore]`d — CI's `build` job has no
  Postgres, per the fix already landed this session): one test per
  eligibility reason (not tradeable, connection inactive, no account for
  this portfolio) plus the happy-path eligible case, against the new
  `/portfolios/:id/venues` endpoint.
- Backend: a new `orders_submit` test asserting a cross-portfolio
  `account_id` is rejected with `400`, and that omitting `account_id`
  remains completely unaffected (regression guard for the "byte-for-byte
  unchanged when not supplied" requirement above).
- Migration: `cargo run -- database migrate` + `database status`, same
  verification shape as every other migration task this session.
- Frontend: no test suite exists for this app (unchanged fact); manual
  browser verification — create a second account on a portfolio via the
  admin console, confirm the dropdown shows both, confirm picking one
  changes the routed venue, confirm the existing single-account behavior
  is unaffected when no second account exists.

## Open questions for the implementation plan

- Exact migration number (next available in `db/migrations/ods/oms/`, `0026`
  as of this writing).
- Whether the venues endpoint's response should also include the
  `broker_code`'s human-readable label or just the code itself — the
  frontend can decide purely from the code for now; a display-name lookup
  is a two-line addition later if it turns out to matter.
