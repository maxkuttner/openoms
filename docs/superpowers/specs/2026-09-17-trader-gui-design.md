# Trader GUI: an order ticket, blotter and positions in the browser

**Date:** 2026-09-17
**Status:** approved design, not yet implemented
**Depends on:** `2026-09-15-human-identity-oidc-sessions-design.md` — this app is the
first consumer of the session auth built there.

## Problem

A trader can reach the OMS three ways: the REST API, the Python client, and
`oms orders list` in a terminal. There is no screen. The cockpit is an operator
console — principals, broker connections, risk limits — and its blotter is
oversight ("who is trading what"), not a place to work from.

Everything a trading screen needs already exists as a grant-scoped endpoint, and
as of the OIDC work those endpoints accept a browser session as readily as an API
key:

| Endpoint | What it gives the screen |
| --- | --- |
| `POST /orders/submit` | order entry |
| `POST /orders/cancel` | cancel |
| `GET /orders` | the caller's own blotter, grant-scoped |
| `GET /orders/{id}` | one order's live state |
| `GET /orders/{id}/events` | the per-order audit trail |
| `GET /portfolios` | portfolios, with this principal's permission flags |
| `GET /portfolios/{id}/positions` | positions with marks and P&L |
| `GET /auth/me` | who am I, and what may I trade |

So this is mostly UI work over a finished API. Mostly — see "What the backend still
needs" below.

## What we are building

A second browser app, served by the OMS at `/trade/`, where a human signs in through
the identity provider and works: an order ticket, a live blotter with cancel, the
per-order timeline, and positions.

**v1 is ticket + blotter + positions + timeline.** Not allocations, not amend/replace
(the API has no replace yet), not charting.

### Placement, and what its isolation actually is

The app is a **separate bundle on the same origin as the cockpit**, sharing a
component library with it.

Be precise about what that does and does not buy. The cockpit is served by the OMS
itself at `/cockpit/`, so a second app at `/trade/` is same-origin: script running in
either can call the other's endpoints with the ambient cookie, and can read the admin
token the cockpit keeps in `localStorage`. **The separation is organisational, not a
security boundary.**

That is acceptable while the cockpit remains a single-operator console on a loopback
bind, and the layout keeps the eventual split cheap — moving `/trade/` to its own
origin later is a deployment change plus CORS, not a rewrite. If a real desk adopts
this, that move is the thing to do, and the admin console should follow the same
discipline.

### Freshness: polling, deliberately

The OMS has no push channel — no SSE, no WebSocket for order updates. The blotter
polls every 2 seconds and positions every 5, using `react-query`'s `refetchInterval`
exactly as the cockpit already does.

A streaming endpoint is worth building and would serve both apps, but it is its own
spec. Bundling it here would make v1 two subsystems instead of one. A 1–2 second lag
is acceptable for discretionary order entry; it is not acceptable for anyone watching
a fast market, and that is the ceiling this choice accepts until streaming lands.

## Out of scope

- **Streaming order updates.** Its own spec, benefiting both apps.
- **Allocations.** The endpoints exist; no v1 screen uses them.
- **Amend/replace.** The API has no replace operation yet.
- **Moving the cockpit to session auth.** It keeps its shared admin token.
- **A frontend test runner.** See "Testing".

## Design

### Code layout: two entry points, one project

`cockpit/vite.config.ts` gains a second entry:

```ts
build: { rollupOptions: { input: { cockpit: 'index.html', trade: 'trade.html' } } }
```

Both apps live in the existing `cockpit/` project and share `src/components` and the
Mantine theme. Vite emits separate bundles; one `package.json`, one dependency tree, no
duplication.

**The API client is the one thing they must NOT share as-is.** `src/api/client.ts`
attaches `Authorization: Bearer <token>` from the admin token in `localStorage`. If the
trade app reused it unchanged, a browser where an operator had signed into the cockpit
would send the **admin token on every trader request** — handing a trader-surface bug
the full admin API. The trade app gets a client that sends **no `Authorization` header
at all** and relies solely on the session cookie (`fetch` sends same-origin cookies by
default). The shared piece is the error handling and the `ApiError` type; the
credential attachment is per-app.

The alternative of a `packages/ui` workspace was rejected as overhead for roughly five
shared components, and duplicating those components was rejected because
`InstrumentSelect` and `OrderTimeline` would immediately drift.

**Vite's `base` is global per build**, so the two apps cannot have asset bases of
`/cockpit/` and `/trade/`. Set `base: '/ui/'` and serve three things from
`src/cockpit.rs`:

- `/ui/*` — the shared asset tree
- `/cockpit/` — the cockpit shell (`index.html`)
- `/trade/` — the trade shell (`trade.html`)

`respond` and `index` in `src/cockpit.rs` currently hardcode `index.html`; they become
shell-parameterised, and a second router is added. The existing cockpit's asset URLs
move from `/cockpit/assets/*` to `/ui/assets/*`, which is invisible to users. The SPA
deep-link fallback behaviour is preserved per shell.

**`/trade/` is mounted only when OIDC is configured.** The trade app authenticates by
session and nothing else, so with no `[auth.oidc]` block it would be a screen that can
never log anyone in. It is gated on the same `file_cfg.oidc().is_some()` condition that
already gates `/auth/*` and `/auth/me`, and 404s otherwise. `/cockpit/` and `/ui/` stay
unconditional.

### Login, and the `return_to` it requires

The app calls `/auth/me` on boot. On 401 it performs a **full-page navigation** to
`/auth/login` — not a fetch, because that endpoint 303s to the identity provider and
XHR cannot usefully follow it.

`auth_api::callback` currently ends with a hardcoded `Redirect::to("/")`, which is
why a successful login lands on `No route found for path: /`. With a second app the
callback must be able to return the trader to `/trade/`.

**`GET /auth/login` accepts an optional `return_to`**, carried in the existing
short-lived flow cookie alongside `state`, `nonce` and `pkce_verifier`. The callback
redirects there instead of `/`. Absent or invalid, the default stays `/`, so nothing
changes for anyone not passing it.

**Validation is the point of this feature, not the redirect.** `return_to` is accepted
only when it is a relative path beginning with exactly one `/`. Rejected:

- an absolute URL (`https://evil.example.com/`)
- a protocol-relative path (`//evil.example.com`) — the classic bypass
- a backslash variant (`/\evil.example.com`)
- anything containing a scheme or authority

Without this check, `/auth/login` becomes an open redirect on the very endpoint that
mints the session, which is worth more to an attacker than most bugs in this system.

### The order ticket

Fields: portfolio, instrument, side, quantity, order type (market/limit), time in
force (day/gtc/ioc/fok), and limit price when the type is limit.

Portfolio options come from `/auth/me`, **filtered to grants where `can_trade` is
true** — a view-only trader is never shown a ticket they cannot submit. Instrument
comes from the new search endpoint through the shared `InstrumentSelect`.

**The idempotency key is the load-bearing detail.** `POST /orders/submit` takes a
client-generated `order_id` which *is* the idempotency key, returns 204 with an empty
body, and answers a repeat with 409.

**Generate that UUID once when the ticket is opened or reset — never per click.** A
double-click, a flaky connection or a retry then lands on the same id, the server
answers 409, and the UI treats that as success. Generating per click turns a
double-click into two live orders.

**Confirm before send.** The primary button does not submit. It opens a confirmation
showing the resolved order in words — "Buy 100 AAPL@XNAS, limit 190.02, day, portfolio
ALPHA" — and only that dialog's button sends. This screen puts real orders at a venue;
one click is not enough separation.

**After submission** the response is empty, so the order is observed by fetching it:
the ticket moves to that order's row in the blotter and polls `GET /orders/{id}` until
it leaves `submitted`.

**Error handling.** Each status means something different to a trader and gets distinct
treatment:

| Status | Meaning | Treatment |
| --- | --- | --- |
| 204 | accepted and routed | success |
| 409 | this `order_id` already exists | **success** — the idempotency story |
| 422 | unknown/inactive instrument, no tradeable mapping, **or pre-trade risk rejection** | show the message verbatim |
| 502 | broker rejected | show the message verbatim |
| 403 | no trade grant | surface as a bug — the portfolio filter should prevent it |
| 503 | no broker adapter configured | operator problem, say so plainly |
| 401 | session expired mid-ticket | redirect to login, **preserving no ticket state** |

"Rejected by pre-trade risk" is not a generic failure — the message carries the reason
and the trader needs it. On 401, restoring ticket state after re-login is how someone
sends an order they no longer intended; the ticket starts empty.

### Blotter and cancel

`GET /orders` is already the caller's own: scoped server-side to the principal's
grants, with `principal_id` in the query string **ignored rather than honoured**, so
the app cannot widen its own view by asking. Same filters as the admin blotter —
status, portfolio, instrument, side, time range, limit/offset. Polls every 2 seconds.

**Cancel is asynchronous and the UI must not lie about it.** `POST /orders/cancel`
returns **202 when the cancel was forwarded to the broker and is still working**, and
204 only when it is done. That distinction is not incidental: the execution stream
finalises `OrderCanceled` on broker confirmation, and that is what fixed the
fill-versus-cancel race.

So on 202 the row reads **"cancelling…"**, not "cancelled", and keeps polling until
the order's own status changes. Rendering a cancel as complete on 202 reintroduces
that race in the interface — where the trader can see it — after it was fixed
underneath.

### Positions

`GET /portfolios/{id}/positions`, polled every 5 seconds, on its own tab: positions
are a different question asked at a different rhythm from order entry.

`mark`, `market_value`, `unrealized_pnl` and `mark_ts` are **nullable** — `None` when
no live quote exists, for an unpriceable or expired contract, or a feed that is down.
**They render as `—`, never as `0`.** A zero mark silently misstates P&L, and
unrealized P&L is exactly the number someone glances at before deciding something.

### The timeline

`OrderTimeline` already exists but hardcodes `/admin/orders/${id}/events`. Its path
becomes a prop: the trade app passes the grant-checked trader route
`/orders/{id}/events`, the cockpit keeps the admin one. Two lines, and it is the
payoff of the shared-project layout.

### Layout

Ticket and blotter on one screen — the ticket's purpose is to put something into the
blotter and watch it. Positions on a separate tab.

## What the backend still needs

This is not purely frontend work. Three additions:

1. **`GET /instruments`** on the authenticated trader router. Smaller than it sounds:
   `admin::list_instruments` already clamps `limit` to 1–200, filters
   `status = 'ACTIVE'`, and matches `symbol ILIKE $1 OR name ILIKE $1`;
   `InstrumentSummary` (`id, symbol, name, venue, asset_class, status`) is exactly what
   a ticket needs. One query, two thin handlers with their own `utoipa` annotations —
   the precedent set by `get_order_events` / `get_order_events_admin`.

   **The query parameter stays `search`**, matching the admin endpoint, because
   `InstrumentSelect` already calls `?limit=50&search=…` with debouncing. Keeping the
   name means the component needs only its base path parameterised.

   The catalog is reference data, identical for every principal, so no grant filtering.

2. **`return_to` on `/auth/login`**, with the validation above.

3. **Shell-parameterised serving** in `src/cockpit.rs`, plus the `/trade/` and `/ui/`
   routes.

## Testing

**The cockpit has no test runner.** There is no frontend test convention in this repo
to follow, and this spec does not introduce one as a side effect — adding Vitest is a
decision about the whole frontend, not something to smuggle into a GUI spec.

**Rust, where the convention exists:**

- `return_to` validation — the set that matters, since it is the open-redirect surface.
  `/trade/` accepted; `//evil.example.com`, `https://evil.example.com`,
  `/\evil.example.com` and scheme-bearing forms each rejected and falling back to the
  default.
- The trader instrument route requires authentication (401 without a credential).
- `#[ignore]`d Postgres test: the instrument search returns only `ACTIVE` rows and
  respects the limit clamp.
- `src/cockpit.rs` shell routing: `/cockpit/` serves the cockpit shell, `/trade/` the
  trade shell, deep links under each fall back to the right one, and a missing asset
  still 404s.

**Frontend:** `tsc --noEmit` as the gate, matching how the cockpit work has been
verified to date, plus a manual checklist against the local Keycloak setup:

1. Visiting `/trade/` unauthenticated redirects to the identity provider and returns
   to `/trade/` after sign-in.
2. Submitting a market order reaches the blotter and leaves `submitted`.
3. Cancelling shows "cancelling…" before it shows cancelled.
4. A position with no live quote renders `—`, not `0`.
5. Double-clicking submit produces exactly one order.
