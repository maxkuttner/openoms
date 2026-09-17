# OMS Cockpit

A React/TypeScript admin webapp for the OMS — set up principals, API keys, portfolio grants,
accounts, broker connections, and risk limits, and watch the order **blotter** ("who is trading
what"). It talks to the OMS admin REST API.

## Stack
Vite + React + TypeScript · [Mantine](https://mantine.dev) (UI) · TanStack Query (data) ·
React Router. No backend code here — just the admin API.

## Run (dev)
1. Start the OMS (from the repo root) so the API is on `http://localhost:3001`.
2. ```bash
   cd cockpit
   npm install
   npm run dev
   ```
3. Open http://localhost:5173. Vite proxies `/api/*` → the OMS (no CORS).
   - Dev with `OMS_ADMIN_AUTH_ENABLED=false`: no token needed.
   - If admin auth is on, click **Set token** (top-right) and paste `OMS_ADMIN_TOKEN`.
   - Point at a different OMS with `OMS_URL=http://host:port npm run dev`.

## Build
```bash
npm run build   # tsc + vite build -> dist/  (base path /ui/)
```
Two entry points build into one `dist/`: `index.html` (the cockpit) and `trade.html` (the
trader app), sharing `dist/assets/`. Vite's `base` is global per build, so the shared asset
base is `/ui/` and neither app owns it.

`dist/` is embedded into the `oms` binary (`include_dir!`, see `src/cockpit.rs`) and served
by the OMS itself: `/cockpit/` serves the cockpit shell, `/trade/` the trade shell (mounted
only when OIDC is configured), and `/ui/*` the assets both of them load.

## Layout
- `src/api/` — `client.ts` (typed fetch + bearer token), `types.ts` (resource shapes),
  `hooks.ts` (TanStack Query helpers).
- `src/components/CrudResource.tsx` — generic list + create/edit/delete used by the simple pages.
- `src/trade/` — the trader app (`/trade/`): its own cookie-only API client, order ticket,
  blotter and positions. It shares `src/components/` and the Mantine theme with the cockpit,
  but never its admin API client.
- `src/pages/` — Principals (+ keys + grants), Portfolios, Accounts, BrokerConnections,
  RiskLimits, Blotter, ApiDocs (embedded [Scalar](https://scalar.com) reference, lazy-loaded,
  fed from the OMS OpenAPI doc).
