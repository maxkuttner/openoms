# OMS web app

The OMS's one web app: trading (order ticket, watchlist, blotter, positions) for every user,
and the **Admin Panel** (users, groups, portfolios, accounts, broker connections, risk limits,
strategies, the order blotter) for admins, behind the user menu.

## Stack
Vite + React + TypeScript · [shadcn/ui](https://ui.shadcn.com) (Radix) + Tailwind CSS ·
TanStack Query (data) · React Router. Dark only; every colour is a CSS variable in
`src/styles/tokens.css` (`check-palette.sh` fails the build on a hex colour anywhere else).

## Run (dev)
1. Start the OMS (from the repo root) so the API is on `http://localhost:3001`.
2. ```bash
   cd cockpit
   npm install
   npm run dev
   ```
3. Open http://localhost:5173. Vite proxies `/api/*` → the OMS (no CORS).
   - Sign in as a named admin (email, password, authenticator code, or SSO). The session
     is a cookie; nothing is kept in `localStorage`.
   - On a fresh install with no named admin yet, the sign-in page also offers the shared
     admin password, only to create that first admin.
   - Point at a different OMS with `OMS_URL=http://host:port npm run dev`.

## Build
```bash
npm run build   # tsc + vite build -> dist/  (base path /ui/)
```
One entry point, `index.html`, builds into `dist/`; assets are served from `/ui/`.

`dist/` is embedded into the `oms` binary (`include_dir!`, see `src/cockpit.rs`) and served
by the OMS itself: `/app/` serves the app shell (any deep link under it too), `/` redirects
there, and `/ui/*` serves the assets.

## Layout
- `src/App.tsx` — the routes. `/login` and `/access` render without a session; everything
  else sits behind `shell/AuthGate.tsx` inside `shell/Shell.tsx`.
- `src/nav.ts` — the sidebar, the account pages and the Admin Panel's tabs (tested).
- `src/shell/` — sign-in gate, sidebar, user menu, ⌘K command menu, Admin Panel frame.
- `src/api/` — `client.ts` (typed fetch, session cookie), `types.ts` (resource shapes),
  `hooks.ts` (TanStack Query helpers), the live-updates socket.
- `src/components/ui/` — shadcn primitives (generated; edit sparingly).
- `src/components/` — app pieces: `form.tsx` (Select, NumberInput, …), `CrudResource.tsx`
  (generic list + create/edit/delete), `trading/` (ticket, blotter, watchlist, search).
- `src/pages/trading/` and `src/pages/admin/` — the screens.
