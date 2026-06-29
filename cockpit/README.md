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
npm run build   # tsc + vite build -> dist/  (base path /cockpit/)
```
`dist/` can be served by any static host, or by the OMS itself (tower-http `ServeDir` at
`/cockpit`, a planned follow-up).

## Layout
- `src/api/` — `client.ts` (typed fetch + bearer token), `types.ts` (resource shapes),
  `hooks.ts` (TanStack Query helpers).
- `src/components/CrudResource.tsx` — generic list + create/edit/delete used by the simple pages.
- `src/pages/` — Principals (+ keys + grants), Portfolios, Accounts, BrokerConnections,
  RiskLimits, Blotter, ApiDocs (embedded [Scalar](https://scalar.com) reference, lazy-loaded,
  fed from the OMS OpenAPI doc).
