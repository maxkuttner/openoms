# One app, shadcn/ui look — design

Status: approved direction, design for implementation. Parent note: `2026-11-16-frontend-direction.md`.

## Intent

Two problems with the UI today, and only these two:

1. **The two-app split.** The admin cockpit (`/cockpit/`) and the trader app (`/trade/`) are separate shells with separate sign-in gates. A trader session opening the cockpit lands on "You don't have access to the admin console".
2. **The look.** Mantine's default look is not what the owner wants. Target: Linear-style, dark, dense.

The work is a **feature-for-feature port**: every existing screen and behaviour survives, nothing new is added except the merged shell and the ⌘K menu.

## Goals

- One SPA, one sign-in, one session, one sidebar that is the same for every user.
- Admin pages live in an Admin Panel reached from the user menu, shown only to admins (Open WebUI model; see Shell).
- Mantine fully removed (`@mantine/*` gone from `package.json`).
- Look: see **Visual tokens** below.
- ⌘K command menu (`cmdk`) listing every page the user may open, plus the existing sign-out.

## Non-goals

- Live-data rework (topic subscriptions, sequence numbers, Zustand, AG Grid). Existing `liveUpdates` socket and `streamHealth` logic carry over unchanged.
- Price charts, order book, any new screen or field.
- Light theme. Dark only; colours live in CSS variables so light can be added later.
- Server API changes, beyond serving one shell.

## Anti-goals

- Any lost behaviour: a form, validation, confirmation, MFA prompt, forced password change, pending screen, setup checklist, task center, jobs indicator, stream-health strip or keyboard flow that works today and not after.
- A half-migrated state on `main` (two styling systems, two looks).

## Constraints

- React 18 + TypeScript + Vite stay. TanStack Query, React Router, API client and session code carry over.
- The build is embedded in the `oms` binary (`src/cockpit.rs`, `include_dir!`); no runtime CDN. Fonts are bundled (`@fontsource/inter`), monospace uses the system stack.
- Tests stay `node --test src/*.test.ts`. Pure modules (`crud`, `lifecycle`, `positions`, `refresh`, `strategies`, `streamHealth`, `venues`, `ibkrImport`, `liveUpdatesCore`) are not touched beyond import paths; their tests must stay green.
- The CSRF origin rule (`src/sessions.rs origin_is_allowed`) and the vite dev proxy stay as they are.
- Single user, prerelease: no compatibility shims for old URLs (see URLs).

## Approaches

- **A. Rewrite in one go (chosen).** New shell on a branch, port all pages, delete Mantine and the second entry point, serve one app. No mixed state on `main`; one large PR, acceptable for a single prerelease user.
- B. Page-by-page migration with Mantine and Tailwind side by side. Smaller PRs, but CSS-reset conflicts, a mixed look for weeks and throwaway glue. Rejected.
- Restyle Mantine to look like Linear. Contradicts the direction note and fights Mantine's density. Rejected.

## Decided by the owner

- Approach A.
- Look: mockup "A · Linear" layout and density, with "C · Trading-dense" colours and monospace numbers.
- Dark only.
- UX modelled on Open WebUI: same sidebar for all, Admin Panel behind the user menu for admins.

## Visual tokens

Defined as shadcn CSS variables in `cockpit/src/styles/tokens.css`. Revised after the click-through (owner's call): the colours are the docs site's brand tokens (`docs-site/src/styles/theme.css`), and a docs test keeps the shared ones equal.

| Token | Value | Source | Use |
|---|---|---|---|
| background | `#13171d` | docs surface | main area |
| sidebar / card | `#0d1014` | docs black | sidebar, panels |
| accent / popover | `#1d232c` | docs raised | hover, active nav, popovers |
| border / input | `#2a2f38` | docs border | borders, inputs |
| foreground | `#ededed` | brand ink | primary text |
| foreground-2 | `#a3a3a3` | brand muted | secondary text |
| muted-foreground | `#737373` | brand faint | labels, hints |
| primary / ring | `#25b083` | docs green (the docs' accent) | buttons, focus, active tab |
| buy | `#25b083` | docs green | buy side, gains, live dot |
| sell / destructive | `#db3440` | brand red | sell side, losses, errors |
| warn | `#ce9a3b` | logo amber | warnings, partial/loading states |
| brand-amber | `#ce9a3b` | logo amber | logo artwork only |

- Font: Inter 14px base (rem-based sizes scale with it); table text 14px, headers text-xs muted, weight 500.
- Numbers: every numeric cell and numeric input uses `ui-monospace, SFMono-Regular, Menlo, monospace` with `tabular-nums`, right-aligned in tables.
- Density: table rows ≈ 26px (5px vertical padding), top bar 38px, sidebar 190px.
- Radius: 5px.
- Clickable elements show the pointer cursor (Tailwind v4 leaves buttons on the arrow).
- Reference mockup (layout and density; colours since revised): `.superpowers/brainstorm/*/content/visual-style-v2.html` (not committed).

## Shell (Open WebUI model)

Owner's decision: the UX follows Open WebUI. Every user gets the same app; admin features appear only for admins, behind the user menu, not in the sidebar.

```
┌ sidebar ─────────┬ top bar: page title · context chips · stream health ┐
│ OpenOMS     ⌘K   │                                                     │
│ Trade            │  page                                               │
│ Positions        │                                                     │
│                  │                                                     │
│ ──────────────── │                                                     │
│ (●) user     ▾   │ ← menu: Settings · Admin Panel (admins) · Sign out  │
└──────────────────┴─────────────────────────────────────────────────────┘

Admin Panel (full page, sidebar stays; tabs on top, sub-nav inside each):
  People         Users · Groups · Sign-in · API
  Trading setup  Portfolios · Accounts · Strategies · Risk limits · Instruments
  Connectivity   Broker connections · Data feeds
  Monitoring     Blotter
```

- **Sidebar:** identical for every user; only the trading pages (Trade, Positions).
- **User menu** at the sidebar foot: **Settings** (own account, password, MFA, Access page), **Admin Panel** (shown only with the admin capability), **Sign out**.
- **Admin Panel:** route `/app/admin/...`; four top tabs grouped as above (grouping is my call), each with a small sub-nav. Blotter and Strategies live here because they read only `/admin/*` endpoints today; a trader-facing version is new work, out of scope. A non-admin opening an admin URL sees an in-shell "no access" message.
- **⌘K:** lists every page the user may open, admin pages included for admins.
- **Landing page:** Trade for everyone (my call). The setup checklist banner shows inside the Admin Panel for admins.

## Auth flow

One gate replaces the cockpit `App` gate and the trader gate: checking → signed out (login page) → pending → forced password change → signed in. "Signed in" no longer requires the admin capability; the capability only drives which routes and nav items exist. MFA prompt, `Frame`-style login and pending screens are ported to the new look.

## URLs and server

- The app is served at `/app/` (my call); `/` redirects there. `/cockpit/*` and `/trade/*` are removed, not redirected (prerelease, no shims).
- One Vite entry (`index.html`); `trade.html` and the second rollup input go away. Asset base stays `/ui/`.
- `src/cockpit.rs`: one router for the shell, mounted unconditionally like today's cockpit router (`main.rs:1108`) (my call). With no sign-in method enabled the login page explains it, as `/auth/methods` already allows. The conditional `trade_router()` mount (`main.rs:1137-1149`) goes away.
- Server-side paths that name the old shells are updated as part of serving one shell (my call; not an API change): `NO_ACCOUNT_URL` in `src/auth_api.rs` → `/app/access?reason=no_account`, the `sanitize_return_to` tests, the admin/trader UI links in `src/setup/status.rs` (become one "UI" link), the startup log line in `main.rs`. Frontend `COCKPIT_HOME`/`TRADE_HOME` (`cockpit/src/login/api.ts`) collapse into one home.
- The Access page keeps its own route, `/app/access`, because the OIDC flow redirects to it by URL; it is also linked from the account menu.
- Update docs-site pages and any scripts that link to `/cockpit/` or `/trade/`.

## Library mapping (my calls)

| Mantine | Replacement |
|---|---|
| core components | shadcn/ui (Radix) + Tailwind |
| `@mantine/form` | react-hook-form + zod (shadcn Form) |
| `@mantine/notifications` | sonner (shadcn toast) |
| `@mantine/hooks` | small local hooks or none |
| modals (e.g. instrument search) | shadcn Dialog; search inside uses `cmdk` |
| tables | shadcn Table; TanStack Table only where a table sorts/filters today |

## Left to the builder

- File layout under `cockpit/src` (merging `api/client.ts` and `trade/api/client.ts` into one client).
- Per-page component choices within the mapping above.
- Order of porting pages inside the branch.
- Whether to rename the `cockpit/` directory (default: keep the name).

## Done when

- Every page from both apps is reachable in the one app with the same behaviour.
- `npm test`, `npm run build`, `cargo test --workspace` pass; no `@mantine` import remains.
- Click-through: admin and non-admin sign-in, pending user, forced password change, MFA, place/cancel an order, positions, every admin CRUD page.
