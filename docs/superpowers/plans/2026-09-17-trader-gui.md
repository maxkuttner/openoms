# Trader GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A browser app at `/trade/` where a trader signs in through the identity provider and sends, watches and cancels orders.

**Architecture:** A second Vite entry point inside the existing `cockpit/` project, sharing components and the Mantine theme but NOT the API client. Served by the OMS as a separate bundle at `/trade/`, authenticated by the session cookie built in the OIDC work. Three backend additions support it: a trader-facing instrument search, a validated `return_to` on `/auth/login`, and shell-parameterised asset serving.

**Tech Stack:** Rust/axum 0.7 backend; React 18 + TypeScript + Mantine 7 + react-query 5 + react-router 7 frontend, built by Vite and embedded with `include_dir!`.

**Spec:** `docs/superpowers/specs/2026-09-17-trader-gui-design.md`

## Global Constraints

- **v1 is ticket + blotter + positions + timeline.** No allocations, no amend/replace, no charting, no streaming.
- **`/trade/` is mounted only when OIDC is configured** — gated on the same `file_cfg.oidc().is_some()` condition that already gates `/auth/*`. `/cockpit/` and `/ui/` stay unconditional.
- **The trade app's API client sends NO `Authorization` header.** It authenticates by session cookie only. Reusing `cockpit/src/api/client.ts` unchanged would send the cockpit's admin token on trader requests.
- **`return_to` is accepted only as a relative path beginning with exactly one `/`.** Absolute URLs, protocol-relative (`//host`), backslash variants (`/\host`) and anything carrying a scheme or authority are rejected, falling back to `/`.
- **The submit idempotency key (`order_id`) is generated once per ticket, never per click.** A repeat lands on 409, which the UI treats as success.
- **Cancel returning 202 means "still working".** The UI shows "cancelling…", never "cancelled", until the order's own status changes.
- **Null marks render as `—`, never `0`.** `mark`, `market_value`, `unrealized_pnl`, `mark_ts` are all nullable.
- Polling only: blotter 2s, positions 5s.
- Any route or response-type change requires regenerating `docs/openapi.json` (`cargo run --quiet -- openapi > docs/openapi.json`); a drift test enforces it.
- The instrument search query parameter is **`search`**, not `q` — matching the admin endpoint so `InstrumentSelect` needs only its base path parameterised.

## File Structure

| File | Responsibility |
| --- | --- |
| `src/auth_api.rs` (modify) | `return_to` validation, carried in the flow cookie, honoured by the callback |
| `src/instruments_api.rs` (new) | The shared instrument-search query plus two thin handlers (trader + admin) |
| `src/admin.rs` (modify) | `list_instruments` delegates to the shared query |
| `src/cockpit.rs` (modify) | Shell-parameterised serving: `/ui/` assets, `/cockpit/` and `/trade/` shells |
| `src/main.rs` (modify) | Mount `/instruments`, mount `/trade/` conditionally |
| `cockpit/vite.config.ts` (modify) | Two entry points, build base `/ui/` |
| `cockpit/trade.html` (new) | The trade app's shell |
| `cockpit/src/main.tsx` (modify) | Explicit `/cockpit/` basename (no longer `BASE_URL`) |
| `cockpit/src/trade/main.tsx` (new) | Trade entry: providers + `/trade/` basename |
| `cockpit/src/trade/api/client.ts` (new) | Cookie-only fetch client |
| `cockpit/src/trade/App.tsx` (new) | Auth gate, layout, routes |
| `cockpit/src/trade/pages/Trade.tsx` (new) | Ticket + blotter on one screen |
| `cockpit/src/trade/pages/Positions.tsx` (new) | Positions tab |
| `cockpit/src/trade/components/OrderTicket.tsx` (new) | The ticket form and its confirm dialog |
| `cockpit/src/trade/components/TradeBlotter.tsx` (new) | Blotter table with cancel |
| `cockpit/src/components/InstrumentSelect.tsx` (modify) | Base path becomes a prop |
| `cockpit/src/components/OrderTimeline.tsx` (modify) | Events path becomes a prop |

---

### Task 1: `return_to` on `/auth/login`

**Files:**
- Modify: `src/auth_api.rs`
- Test: in-module `#[cfg(test)]` in `src/auth_api.rs`

**Interfaces:**
- Consumes: the existing `FlowState { state, nonce, pkce_verifier }`, `flow_cookie`, `flow_from_cookie`.
- Produces:
  - `pub fn sanitize_return_to(raw: Option<&str>) -> String` — returns the validated path or `"/"`.
  - `FlowState` gains `pub return_to: String`.

**This is the security-critical task in the plan.** Without the validation, `/auth/login` becomes an open redirect on the endpoint that mints the session.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/auth_api.rs`:

```rust
    #[test]
    fn a_relative_path_is_kept() {
        assert_eq!(sanitize_return_to(Some("/trade/")), "/trade/");
        assert_eq!(sanitize_return_to(Some("/trade/orders?status=filled")), "/trade/orders?status=filled");
    }

    #[test]
    fn a_missing_return_to_falls_back_to_the_root() {
        assert_eq!(sanitize_return_to(None), "/");
        assert_eq!(sanitize_return_to(Some("")), "/");
    }

    #[test]
    fn an_absolute_url_is_refused() {
        assert_eq!(sanitize_return_to(Some("https://evil.example.com/")), "/");
        assert_eq!(sanitize_return_to(Some("http://evil.example.com/")), "/");
    }

    #[test]
    fn a_protocol_relative_path_is_refused() {
        // The classic open-redirect bypass: the browser reads "//host" as a
        // scheme-relative URL and leaves the site entirely.
        assert_eq!(sanitize_return_to(Some("//evil.example.com")), "/");
        assert_eq!(sanitize_return_to(Some("//evil.example.com/path")), "/");
    }

    #[test]
    fn a_backslash_variant_is_refused() {
        // Some browsers normalise a backslash to a forward slash, making this
        // another way to write "//".
        assert_eq!(sanitize_return_to(Some("/\\evil.example.com")), "/");
        assert_eq!(sanitize_return_to(Some("\\\\evil.example.com")), "/");
    }

    #[test]
    fn a_path_that_does_not_start_with_a_slash_is_refused() {
        assert_eq!(sanitize_return_to(Some("trade/")), "/");
        assert_eq!(sanitize_return_to(Some("javascript:alert(1)")), "/");
    }

    #[test]
    fn a_control_character_is_refused() {
        // A newline or tab can be used to smuggle a second header or confuse a
        // proxy; a legitimate path never contains one.
        assert_eq!(sanitize_return_to(Some("/trade/\nSet-Cookie: x=1")), "/");
        assert_eq!(sanitize_return_to(Some("/trade/\tfoo")), "/");
    }

    #[test]
    fn the_flow_cookie_carries_the_return_to() {
        let policy = crate::sessions::cookie_policy("localhost:3001", None);
        let flow = FlowState {
            state: "st-1".into(),
            nonce: "n-1".into(),
            pkce_verifier: "v-1".into(),
            return_to: "/trade/".into(),
        };

        let header = flow_cookie(&policy, &flow);
        let mut headers = HeaderMap::new();
        headers.insert("cookie", header.split(';').next().unwrap().parse().unwrap());

        assert_eq!(flow_from_cookie(&headers, &policy).expect("flow").return_to, "/trade/");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms auth_api`
Expected: FAIL — `cannot find function sanitize_return_to`, and `FlowState` missing field `return_to`.

- [ ] **Step 3: Implement**

```rust
/// Validate a caller-supplied post-login destination.
///
/// Only a same-site relative path is allowed. Everything else falls back to
/// `/`. This is the whole security value of the feature: without it,
/// `/auth/login` is an open redirect on the endpoint that mints the session,
/// which is worth more to an attacker than most bugs in this system.
pub fn sanitize_return_to(raw: Option<&str>) -> String {
    const DEFAULT: &str = "/";

    let Some(candidate) = raw else { return DEFAULT.to_string() };

    // Must be a path, not a URL, and not scheme-relative.
    let starts_with_single_slash = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.starts_with("/\\");
    if !starts_with_single_slash {
        return DEFAULT.to_string();
    }

    // A control character can smuggle a header or confuse a proxy; a real path
    // never has one.
    if candidate.chars().any(|c| c.is_control()) {
        return DEFAULT.to_string();
    }

    candidate.to_string()
}
```

Add `pub return_to: String` to `FlowState` (it serialises into the existing flow cookie with no other change). In the `login` handler, read `return_to` from the query string — add a `#[derive(Deserialize)] pub struct LoginParams { pub return_to: Option<String> }` and a `Query<LoginParams>` extractor — and store `sanitize_return_to(params.return_to.as_deref())` in the `FlowState`. In `callback`, replace `Redirect::to("/")` with `Redirect::to(&flow.return_to)`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin oms auth_api`
Expected: PASS.

- [ ] **Step 5: Regenerate the OpenAPI spec and run the whole suite**

Run: `cargo run --quiet -- openapi > docs/openapi.json`
Run: `cargo test --bin oms`
Expected: PASS, including `the_committed_openapi_spec_is_current`.

- [ ] **Step 6: Commit**

```bash
git add src/auth_api.rs docs/openapi.json
git commit -m "feat(auth): honour a validated return_to after login"
```

---

### Task 2: Trader-facing instrument search

**Files:**
- Create: `src/instruments_api.rs`
- Modify: `src/admin.rs` (make `list_instruments` delegate), `src/main.rs` (add `mod instruments_api;`, mount the route, register in `ApiDoc`)
- Test: in-module `#[cfg(test)]` in `src/instruments_api.rs`

**Interfaces:**
- Consumes: `admin::InstrumentSummary { id, symbol, name, venue, asset_class, status }`, `admin::InstrumentSearch { search, limit }`.
- Produces:
  - `pub async fn search_instruments(pool: &PgPool, search: Option<&str>, limit: Option<i64>) -> Result<Vec<InstrumentSummary>, sqlx::Error>`
  - `pub async fn list_instruments_for_trader(State<AppState>, Query<InstrumentSearch>) -> Result<Json<Vec<InstrumentSummary>>, ApiError>` mounted at `GET /instruments`

Follows the precedent of `get_order_events` / `get_order_events_admin`: one query, two thin handlers with their own `utoipa` annotations.

- [ ] **Step 1: Write the failing tests**

Create `src/instruments_api.rs` with the query stubbed as `todo!()`, then:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog is reference data — identical for every principal — so the
    /// trader endpoint applies no grant filtering. What it MUST do is match the
    /// admin endpoint's contract: only ACTIVE rows, and a clamped limit.
    ///
    /// Run with: cargo test -- --ignored
    /// Requires: a live Postgres reachable via the usual POSTGRES_* config.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn only_active_instruments_are_returned() {
        let pool = test_pool().await;
        let active = seed_instrument(&pool, "ZZTESTA", "ACTIVE").await;
        let inactive = seed_instrument(&pool, "ZZTESTI", "INACTIVE").await;

        let rows = search_instruments(&pool, Some("ZZTEST"), None).await.expect("search");
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

        assert!(ids.contains(&active), "an ACTIVE instrument must be findable");
        assert!(!ids.contains(&inactive), "an INACTIVE instrument must not be");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_limit_is_clamped_to_the_documented_range() {
        let pool = test_pool().await;

        let none = search_instruments(&pool, Some("ZZNOMATCHATALL"), Some(9_999)).await.expect("high");
        assert!(none.len() <= 200, "limit must clamp to 200");

        // A zero or negative limit must not produce an error or an unbounded query.
        let zero = search_instruments(&pool, Some("ZZNOMATCHATALL"), Some(0)).await;
        assert!(zero.is_ok(), "a zero limit clamps rather than failing");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_search_matches_symbol_or_name() {
        let pool = test_pool().await;
        let id = seed_instrument(&pool, "ZZNAMEHUNT", "ACTIVE").await;

        // seed_instrument sets name = "Test <symbol>", so both paths are covered.
        let by_symbol = search_instruments(&pool, Some("ZZNAMEHUNT"), None).await.expect("symbol");
        let by_name = search_instruments(&pool, Some("Test ZZNAMEHUNT"), None).await.expect("name");

        assert!(by_symbol.iter().any(|r| r.id == id));
        assert!(by_name.iter().any(|r| r.id == id));
    }

    // ── test plumbing ────────────────────────────────────────────────────────

    /// `main` loads .env before resolving config; a test binary does not, so
    /// without this the test silently resolves a different database than the
    /// server runs against. The runtime role carries `search_path = oms, public`
    /// (db/access/roles.sql); these are admin credentials, so set it here.
    async fn test_pool() -> sqlx::PgPool {
        use crate::setup::database::config;
        dotenvy::dotenv().ok();
        let cfg = config::resolve(config::PostgresOverrides::default());
        sqlx::postgres::PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&cfg.url())
            .await
            .expect("connect")
    }

    /// Instruments live in `public`, not `oms`. Returns the new row's id.
    async fn seed_instrument(pool: &sqlx::PgPool, symbol: &str, status: &str) -> i64 {
        let unique = format!("{symbol}{}", &uuid::Uuid::new_v4().to_string()[..8]);
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO public.instrument (symbol, name, venue, asset_class, instrument_class, status) \
             VALUES ($1, $2, 'XNAS', 'EQUITY', 'STOCK', $3) RETURNING id",
        )
        .bind(&unique)
        .bind(format!("Test {unique}"))
        .bind(status)
        .fetch_one(pool)
        .await
        .expect("seed instrument")
    }
}
```

Before writing these, run `sed -n '1,45p' db/migrations/ods/public/0003_CREATE_INSTRUMENT_TABLE.sql` and adjust the INSERT to the table's actual NOT NULL columns — the seed above assumes `symbol, name, venue, asset_class, instrument_class, status` and will fail loudly if the schema needs more. Fix the INSERT, not the assertions.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms instruments_api -- --ignored`
Expected: FAIL — `not yet implemented`.

- [ ] **Step 3: Implement the shared query and the trader handler**

```rust
//! Instrument catalog lookup.
//!
//! The catalog is reference data — the same symbols, venues and names for every
//! principal — so the trader endpoint applies no grant filtering. It exists
//! separately from the admin one only so an order ticket can search without an
//! admin token.

use axum::{extract::{Query, State}, Json};
use sqlx::PgPool;

use crate::admin::{InstrumentSearch, InstrumentSummary};
use crate::app_state::AppState;
use crate::handlers::ApiError;

/// The one query behind both the admin and trader endpoints.
pub async fn search_instruments(
    pool: &PgPool,
    search: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<InstrumentSummary>, sqlx::Error> {
    let pattern = search.map(|s| format!("%{s}%"));
    let limit = limit.unwrap_or(50).clamp(1, 200);

    sqlx::query_as::<_, InstrumentSummary>(
        "SELECT id, symbol, name, venue, asset_class, status \
         FROM public.instrument \
         WHERE status = 'ACTIVE' AND ($1::text IS NULL OR symbol ILIKE $1 OR name ILIKE $1) \
         ORDER BY symbol \
         LIMIT $2",
    )
    .bind(pattern)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[utoipa::path(
    get, path = "/instruments", tag = "orders",
    params(InstrumentSearch),
    responses(
        (status = 200, description = "Matching active instruments", body = [InstrumentSummary]),
        (status = 401, description = "No credential"),
    ),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_instruments_for_trader(
    State(state): State<AppState>,
    Query(params): Query<InstrumentSearch>,
) -> Result<Json<Vec<InstrumentSummary>>, ApiError> {
    search_instruments(state.pool(), params.search.as_deref(), params.limit)
        .await
        .map(Json)
        .map_err(|err| ApiError {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to search instruments: {err:?}"),
        })
}
```

Open `src/admin.rs`, find `list_instruments`, and replace its query body with a call to `crate::instruments_api::search_instruments(state.pool(), params.search.as_deref(), params.limit)`. Its `utoipa` annotation, path and response type stay exactly as they are. If `InstrumentSummary` does not already derive `sqlx::FromRow`, add it.

Add `mod instruments_api;` to the flat module list at the top of `src/main.rs`; mount `.route("/instruments", get(instruments_api::list_instruments_for_trader))` on `orders_router` (the one carrying `auth_middleware`); add `instruments_api::list_instruments_for_trader` to `ApiDoc`'s `paths(...)`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --bin oms instruments_api -- --ignored`
Expected: PASS, 3 tests.
Run: `cargo test --bin oms`
Expected: PASS — the admin endpoint's own behaviour is unchanged.

- [ ] **Step 5: Verify authentication is actually required**

Start the server, then:

```bash
curl -s -o /dev/null -w "%{http_code}\n" "http://localhost:3001/instruments?search=A"
```

Expected: `401`. If it returns 200, the route was mounted on the wrong router — it must sit behind `auth_middleware`.

- [ ] **Step 6: Regenerate the spec and commit**

```bash
cargo run --quiet -- openapi > docs/openapi.json
git add src/instruments_api.rs src/admin.rs src/main.rs docs/openapi.json
git commit -m "feat(instruments): let an authenticated trader search the catalog"
```

---

### Task 3: Serve two shells from one asset tree

**Files:**
- Modify: `src/cockpit.rs`, `src/main.rs`
- Test: in-module `#[cfg(test)]` in `src/cockpit.rs`

**Interfaces:**
- Produces:
  - `pub fn respond(shell: &str, path: &str) -> Response` — `shell` is the fallback HTML filename
  - `pub fn router<S>() -> Router<S>` — serves `/ui/*`, `/cockpit`, `/cockpit/`, `/cockpit/*path`
  - `pub fn trade_router<S>() -> Router<S>` — serves `/trade`, `/trade/`, `/trade/*path`

`asset_for` and `respond` currently hardcode `index.html` as both the default and the SPA fallback. They become shell-parameterised so `/trade/` falls back to `trade.html`.

- [ ] **Step 1: Write the failing tests**

The existing tests in `src/cockpit.rs` (`a_bundled_build_serves_the_shell`, `deep_links_fall_back_to_the_shell`, `a_missing_asset_stays_missing`) call `respond(path)` with one argument and must be updated to `respond("index.html", path)`. Then add:

```rust
    #[test]
    fn a_trade_deep_link_falls_back_to_the_trade_shell_not_the_cockpit_one() {
        if !is_bundled() {
            return; // source build: nothing embedded to serve
        }
        let cockpit = respond("index.html", "orders");
        let trade = respond("trade.html", "orders");

        assert_eq!(cockpit.status(), StatusCode::OK);
        assert_eq!(trade.status(), StatusCode::OK);
        // Both are HTML shells, but they must not be the SAME shell — serving the
        // cockpit's bundle under /trade/ would load the admin app at the trader's
        // URL, with the admin token attached.
        assert_ne!(
            body_bytes(cockpit),
            body_bytes(trade),
            "each app must fall back to its own shell"
        );
    }

    #[test]
    fn a_missing_asset_stays_missing_for_either_shell() {
        if !is_bundled() {
            return;
        }
        assert_eq!(respond("trade.html", "assets/nope.js").status(), StatusCode::NOT_FOUND);
    }
```

Add a `body_bytes(resp: Response) -> Vec<u8>` test helper using `axum::body::to_bytes`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin oms cockpit`
Expected: FAIL to compile — `respond` takes 1 argument, 2 supplied.

- [ ] **Step 3: Implement**

Change the signatures so the shell is explicit:

```rust
pub fn asset_for(shell: &str, path: &str) -> Option<Asset> {
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { shell } else { rel };

    if let Some(file) = DIST.get_file(rel) {
        let cache_control = if rel.starts_with("assets/") { IMMUTABLE } else { NO_CACHE };
        return Some(Asset {
            body: file.contents(),
            content_type: mime_guess::from_path(rel).first_or_octet_stream().to_string(),
            cache_control,
        });
    }

    // A miss with no extension is a client-side route; only the shell can answer
    // it. Which shell depends on which app the URL belongs to.
    if std::path::Path::new(rel).extension().is_none() {
        let shell_file = DIST.get_file(shell)?;
        return Some(Asset {
            body: shell_file.contents(),
            content_type: "text/html".to_string(),
            cache_control: NO_CACHE,
        });
    }

    None
}

pub fn respond(shell: &str, path: &str) -> Response { /* unchanged body, passing shell through */ }
```

`is_bundled()` keeps checking `index.html`. Add handlers and a second router:

```rust
async fn trade_index() -> Response { respond("trade.html", "") }
async fn trade_asset(UrlPath(path): UrlPath<String>) -> Response { respond("trade.html", &path) }
async fn redirect_to_trade_slash() -> Redirect { Redirect::temporary("/trade/") }

/// The shared asset tree. Both bundles emit into `dist/assets/`, and Vite's
/// `base` points every generated URL at `/ui/`.
async fn ui_asset(UrlPath(path): UrlPath<String>) -> Response { respond("index.html", &path) }

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/ui/*path", get(ui_asset))
        .route("/cockpit", get(redirect_to_slash))
        .route("/cockpit/", get(index))
        .route("/cockpit/*path", get(asset))
}

pub fn trade_router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/trade", get(redirect_to_trade_slash))
        .route("/trade/", get(trade_index))
        .route("/trade/*path", get(trade_asset))
}
```

In `src/main.rs`, merge `cockpit::trade_router()` **only inside the block already gated on OIDC being configured** — the same `if let Some(...)` that mounts `/auth/*`. The trade app authenticates by session and nothing else, so without OIDC it would be a screen nobody can log into.

- [ ] **Step 4: Run the tests**

Run: `cargo test --bin oms cockpit`
Expected: PASS.
Run: `cargo test --bin oms`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/cockpit.rs src/main.rs
git commit -m "feat(cockpit): serve a second shell at /trade/ from a shared asset tree"
```

---

### Task 4: The trade app shell, its client, and the auth gate

**Files:**
- Create: `cockpit/trade.html`, `cockpit/src/trade/main.tsx`, `cockpit/src/trade/api/client.ts`, `cockpit/src/trade/App.tsx`
- Modify: `cockpit/vite.config.ts`, `cockpit/src/main.tsx`

**Interfaces:**
- Produces:
  - `tradeApi.get<T>(path)`, `.post<T>(path, body)`, `.del(path)` — cookie-only
  - `ApiError { status, message }` re-exported from the trade client
  - `type Me = { principal_id: string; code: string; display_name: string | null; portfolios: GrantedPortfolio[] }`

- [ ] **Step 1: Two entry points and a shared asset base**

`cockpit/vite.config.ts` — the existing `base` is `command === "build" ? "/cockpit/" : "/"`. Change the build value to `/ui/` and add the second input:

```ts
export default defineConfig(({ command }) => ({
  plugins: [react()],
  // Both bundles share one asset tree; each app's SHELL is served at its own
  // path (/cockpit/, /trade/) by src/cockpit.rs. Vite's base is global per
  // build, so it cannot be either app's path.
  base: command === "build" ? "/ui/" : "/",
  build: {
    rollupOptions: { input: { cockpit: "index.html", trade: "trade.html" } },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target, changeOrigin: true, rewrite: (p) => p.replace(/^\/api/, "") },
    },
  },
}));
```

**This breaks an assumption in `cockpit/src/main.tsx`**, which sets the router basename from `import.meta.env.BASE_URL`. That is now `/ui/` in a build — the asset base, not the app's path. Each app must state its own:

```tsx
// BASE_URL is the shared ASSET base (/ui/), not this app's path. The cockpit's
// shell is served at /cockpit/, so that is its router basename.
const basename = import.meta.env.DEV ? "/" : "/cockpit/";
```

`cockpit/trade.html`, copied from `index.html` with a different title and entry:

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <link rel="icon" type="image/svg+xml" href="%BASE_URL%favicon.svg" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <link rel="preconnect" href="https://fonts.googleapis.com" />
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin />
    <link
      href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;600;700;800&display=swap"
      rel="stylesheet"
    />
    <title>openOMS Trade</title>
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="/src/trade/main.tsx"></script>
  </body>
</html>
```

- [ ] **Step 2: The cookie-only client**

`cockpit/src/trade/api/client.ts`:

```ts
// The trade app authenticates by session cookie and NOTHING else.
//
// It deliberately does not reuse cockpit/src/api/client.ts: that one attaches
// `Authorization: Bearer <admin token>` from localStorage. On a browser where an
// operator has signed into the cockpit, reusing it would put the admin token on
// every trader request — handing any bug on this surface the full admin API.
// fetch sends same-origin cookies by default, so the session needs no header.

export const API_BASE = import.meta.env.DEV ? "/api" : "";

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  if (!res.ok) {
    // 401 means the session is gone. A full-page navigation is required because
    // /auth/login answers 303 to the identity provider, which fetch cannot follow
    // usefully.
    if (res.status === 401) {
      window.location.href = `/auth/login?return_to=${encodeURIComponent("/trade/")}`;
      // Never resolves; the navigation is already underway.
      return new Promise<T>(() => {});
    }
    const text = await res.text().catch(() => "");
    throw new ApiError(res.status, text || `${method} ${path} failed (${res.status})`);
  }
  if (res.status === 204) return undefined as T;
  const text = await res.text();
  return text ? (JSON.parse(text) as T) : (undefined as T);
}

export const tradeApi = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body: unknown) => request<T>("POST", path, body),
  del: (path: string) => request<void>("DELETE", path),
};
```

- [ ] **Step 3: Entry point and auth gate**

`cockpit/src/trade/main.tsx` mirrors `cockpit/src/main.tsx` — same providers, same theme — with `const basename = import.meta.env.DEV ? "/" : "/trade/";` and rendering `<TradeApp />`.

`cockpit/src/trade/App.tsx`:

```tsx
import { useQuery } from "@tanstack/react-query";
import { AppShell, Group, Loader, Text, Tabs } from "@mantine/core";
import { Routes, Route, Navigate, useNavigate, useLocation } from "react-router-dom";
import { tradeApi } from "./api/client";
import { TradePage } from "./pages/Trade";
import { PositionsPage } from "./pages/Positions";

export type GrantedPortfolio = {
  portfolio_id: string;
  code: string;
  name: string;
  status: string;
  base_currency: string | null;
  can_trade: boolean;
  can_view: boolean;
  can_allocate: boolean;
};

export type Me = {
  principal_id: string;
  code: string;
  display_name: string | null;
  portfolios: GrantedPortfolio[];
};

export function TradeApp() {
  // A 401 here redirects to the identity provider from inside the client, so
  // this query either resolves with an identity or the page navigates away.
  const me = useQuery<Me>({ queryKey: ["/auth/me"], queryFn: () => tradeApi.get<Me>("/auth/me") });

  if (me.isLoading) return <Loader />;
  if (me.error) return <Text c="red">Could not load your session.</Text>;

  return (
    <AppShell header={{ height: 48 }} padding="md">
      <AppShell.Header>
        <Group h="100%" px="md" justify="space-between">
          <Text fw={600}>openOMS</Text>
          <Text size="sm" c="dimmed">{me.data?.display_name ?? me.data?.code}</Text>
        </Group>
      </AppShell.Header>
      <AppShell.Main>
        <Routes>
          <Route path="/" element={<TradePage me={me.data!} />} />
          <Route path="/positions" element={<PositionsPage me={me.data!} />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </AppShell.Main>
    </AppShell>
  );
}
```

Create `pages/Trade.tsx` and `pages/Positions.tsx` as stubs rendering their own name for now; Tasks 6–8 fill them.

- [ ] **Step 4: Typecheck and build**

Run: `cd cockpit && npx tsc --noEmit`
Expected: clean.
Run: `cd cockpit && npm run build`
Expected: succeeds, and `cockpit/dist/` contains BOTH `index.html` and `trade.html` plus a shared `assets/` directory. Verify with `ls cockpit/dist`.

- [ ] **Step 5: Verify end to end against the running server**

```bash
cargo build && cargo run --quiet &
curl -s -o /dev/null -w "cockpit %{http_code}\n" http://localhost:3001/cockpit/
curl -s -o /dev/null -w "trade   %{http_code}\n" http://localhost:3001/trade/
```

Expected with OIDC configured: both 200. Expected with no `[auth.oidc]` block: `/cockpit/` 200 and `/trade/` **404**.

- [ ] **Step 6: Commit**

```bash
git add cockpit/vite.config.ts cockpit/trade.html cockpit/src/main.tsx cockpit/src/trade
git commit -m "feat(trade): app shell, cookie-only client, and the login gate"
```

---

### Task 5: Parameterise the two shared components

**Files:**
- Modify: `cockpit/src/components/InstrumentSelect.tsx`, `cockpit/src/components/OrderTimeline.tsx`, and their cockpit call sites

**Interfaces:**
- Produces:
  - `<InstrumentSelect ... basePath?: string />` — defaults to `/admin/instruments`
  - `<OrderTimeline orderId eventsPath?: string />` — defaults to `/admin/orders`

This is the payoff of keeping one project: the timeline component built for the audit trail gets reused rather than copied.

- [ ] **Step 1: Make the paths props**

`InstrumentSelect` currently builds `` `/admin/instruments?limit=50${...&search=...}` `` and keys its query on `["/admin/instruments", debounced]`. Add a `basePath = "/admin/instruments"` prop, use it in both the URL and the query key (so the two apps do not share a cache entry), and keep the debouncing exactly as it is.

It also imports the cockpit's `api`. Add an `apiGet?: (path: string) => Promise<unknown>` prop defaulting to the cockpit client, so the trade app can pass `tradeApi.get`. Without this the component would send the admin token from the trade app — the exact failure the separate client exists to prevent.

`OrderTimeline` hardcodes `/admin/orders/${orderId}/events`. Add `eventsPath = "/admin/orders"` and the same `apiGet` prop.

- [ ] **Step 2: Leave every cockpit call site working**

Both defaults match today's behaviour, so the cockpit's existing usages need no change. Confirm that is true rather than assuming: `grep -rn "InstrumentSelect\|OrderTimeline" cockpit/src --include=*.tsx`.

- [ ] **Step 3: Typecheck**

Run: `cd cockpit && npx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: Verify the cockpit still works**

With the server running, open `http://localhost:3001/cockpit/`, go to the Blotter, click an order and confirm the timeline still renders, then open the Instruments picker and confirm search still returns rows.

- [ ] **Step 5: Commit**

```bash
git add cockpit/src/components
git commit -m "refactor(cockpit): let the shared components target either API surface"
```

---

### Task 6: The blotter, with an honest cancel

**Files:**
- Create: `cockpit/src/trade/components/TradeBlotter.tsx`
- Modify: `cockpit/src/trade/pages/Trade.tsx`

**Interfaces:**
- Consumes: `tradeApi`, `Me`, `OrderTimeline`.
- Produces: `<TradeBlotter portfolios={GrantedPortfolio[]} />`

- [ ] **Step 1: The table**

`useQuery` on `/orders` with `refetchInterval: 2000`. Columns: time, instrument, side, type, status, qty, cum, leaves, avg px, and a cancel action. Status badge colours reuse the cockpit blotter's map. Clicking a row opens a Mantine `Drawer` containing `<OrderTimeline orderId={...} eventsPath="/orders" apiGet={tradeApi.get} />`.

`GET /orders` is already scoped to the caller's grants server-side, and `principal_id` in the query string is ignored rather than honoured — so no client-side filtering by principal is needed or possible.

- [ ] **Step 2: Cancel, and the 202 distinction**

```tsx
// POST /orders/cancel answers 202 when the cancel was FORWARDED to the broker
// and is still working, and 204 only when it is done. The execution stream
// finalises OrderCanceled on broker confirmation — that asymmetry is what fixed
// the fill-versus-cancel race, so the UI must not flatten it back out by
// claiming the order is cancelled the moment the request returns.
const [cancelling, setCancelling] = useState<Set<string>>(new Set());

async function cancel(orderId: string) {
  setCancelling((s) => new Set(s).add(orderId));
  try {
    await tradeApi.post("/orders/cancel", { order_id: orderId });
    // Either 202 or 204: in both cases the row keeps polling until the ORDER's
    // own status changes. We never render "cancelled" on our own authority.
    notifications.show({ message: "Cancel sent", color: "blue" });
  } catch (err) {
    setCancelling((s) => { const n = new Set(s); n.delete(orderId); return n; });
    notifyError(err);
  }
}
```

A row whose id is in `cancelling` and whose status is not yet terminal renders the status as **"cancelling…"**. Once the polled status reaches `canceled` (or `filled` — the cancel lost the race, which is exactly the case this honesty exists for), drop it from the set and show the real status.

- [ ] **Step 3: Typecheck**

Run: `cd cockpit && npx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: Verify by hand**

With the Keycloak setup running and signed in at `/trade/`: submit an order via the Python client or `curl`, confirm it appears in the blotter within ~2s, click it and confirm the timeline renders, then cancel it and confirm the row reads "cancelling…" before it reads canceled.

- [ ] **Step 5: Commit**

```bash
git add cockpit/src/trade
git commit -m "feat(trade): blotter with per-order timeline and an honest cancel"
```

---

### Task 7: The order ticket

**Files:**
- Create: `cockpit/src/trade/components/OrderTicket.tsx`
- Modify: `cockpit/src/trade/pages/Trade.tsx`

**Interfaces:**
- Consumes: `tradeApi`, `GrantedPortfolio`, `InstrumentSelect`.
- Produces: `<OrderTicket portfolios={GrantedPortfolio[]} onSubmitted={(orderId: string) => void} />`

- [ ] **Step 1: The form**

Fields: portfolio (options = `portfolios.filter(p => p.can_trade)`), instrument (`<InstrumentSelect basePath="/instruments" apiGet={tradeApi.get} />`), side (buy/sell), quantity, order type (market/limit), time in force (day/gtc/ioc/fok), and limit price shown only when type is limit and required then.

- [ ] **Step 2: The idempotency key — generated once**

```tsx
// POST /orders/submit takes a client-generated order_id which IS the idempotency
// key: the server answers a repeat with 409. Generate it when the ticket is
// opened or reset — NEVER per click. A double-click, a retry or a flaky
// connection then lands on the same id and is absorbed; generating per click
// turns a double-click into two live orders.
const [orderId, setOrderId] = useState(() => crypto.randomUUID());
// ...after a successful submit, and on explicit reset:
setOrderId(crypto.randomUUID());
```

- [ ] **Step 3: Confirm before send**

The primary button opens a Mantine `Modal` summarising the order in words — `Buy 100 AAPL@XNAS · limit 190.02 · day · portfolio ALPHA` — and only the modal's confirm button calls the API. This screen puts real orders at a venue; one click is not enough separation.

- [ ] **Step 4: Submit and map every error**

```tsx
try {
  await tradeApi.post("/orders/submit", {
    order_id: orderId,
    portfolio_id: portfolio,
    instrument_id: instrument,
    side, quantity, order_type: orderType,
    time_in_force: tif,
    limit_price: orderType === "limit" ? limitPrice : undefined,
  });
  onSubmitted(orderId);
  setOrderId(crypto.randomUUID());
} catch (err) {
  if (err instanceof ApiError) {
    switch (err.status) {
      case 409:
        // This exact order_id already exists — the idempotency contract. The
        // order is live; this is success, not failure.
        onSubmitted(orderId);
        setOrderId(crypto.randomUUID());
        break;
      case 422:
        // Unknown/inactive instrument, no tradeable mapping, OR a pre-trade risk
        // rejection. The message carries the reason and the trader needs it
        // verbatim — "notional limit breached" is not a generic failure.
        notifications.show({ color: "red", title: "Rejected", message: err.message });
        break;
      case 502:
        notifications.show({ color: "red", title: "Broker rejected", message: err.message });
        break;
      case 503:
        notifications.show({ color: "orange", title: "No broker configured", message: "An operator needs to configure a broker connection." });
        break;
      case 403:
        // The portfolio list is already filtered to can_trade, so this is a bug,
        // not a routine outcome.
        notifications.show({ color: "red", title: "Not permitted", message: "This portfolio is not tradeable by your account. Please report this." });
        break;
      default:
        notifyError(err);
    }
  }
}
```

401 needs no case: the client redirects to login on 401, and the ticket starts empty afterwards. Restoring ticket state after re-login is how someone sends an order they no longer intended.

- [ ] **Step 5: Typecheck**

Run: `cd cockpit && npx tsc --noEmit`
Expected: clean.

- [ ] **Step 6: Verify by hand**

Signed in at `/trade/`: submit a market order and confirm it appears in the blotter; submit one that breaches a risk limit and confirm the rejection message appears verbatim; **double-click confirm and verify exactly one order exists**.

- [ ] **Step 7: Commit**

```bash
git add cockpit/src/trade
git commit -m "feat(trade): order ticket with confirmation and idempotent submit"
```

---

### Task 8: Positions

**Files:**
- Modify: `cockpit/src/trade/pages/Positions.tsx`

**Interfaces:**
- Consumes: `tradeApi`, `Me`.

- [ ] **Step 1: The table**

A portfolio selector (from `me.portfolios` where `can_view`), then `useQuery` on `/portfolios/${id}/positions` with `refetchInterval: 5000`. Columns: instrument, net qty, avg cost, mark, market value, unrealized P&L, realized P&L, mark time.

- [ ] **Step 2: Null marks render as an em dash**

```tsx
// mark, market_value, unrealized_pnl and mark_ts are null when no live quote
// exists — an unpriceable or expired contract, or a feed that is down. Render
// them as "—", NEVER as 0: a zero mark silently misstates P&L, and unrealized
// P&L is exactly the number someone glances at before deciding something.
const num = (v: number | null | undefined) => (v === null || v === undefined ? "—" : v);
```

Apply it to every nullable column. Do not default them in the type, and do not use `?? 0` anywhere in this file.

- [ ] **Step 3: Typecheck**

Run: `cd cockpit && npx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: Verify by hand**

Open the Positions tab for a portfolio holding an instrument with no live quote and confirm the mark column shows `—` rather than `0`.

- [ ] **Step 5: Commit**

```bash
git add cockpit/src/trade
git commit -m "feat(trade): positions with honest empty marks"
```

---

### Task 9: Documentation and the final check

**Files:**
- Modify: `readme.md`, `docs/openapi.json`

- [ ] **Step 1: README**

Add a short section after the OIDC one: the trade app lives at `/trade/`, appears only when `[auth.oidc]` is configured, signs in through the identity provider, and shows only the portfolios the signed-in principal has been granted. Match the README's existing tone; read it first.

- [ ] **Step 2: Regenerate the spec**

Run: `cargo run --quiet -- openapi > docs/openapi.json`
Run: `cargo test --bin oms`
Expected: PASS including the drift test.

- [ ] **Step 3: Full manual checklist**

Against the local Keycloak setup, signed in as `trader`:

1. Visiting `/trade/` unauthenticated redirects to the identity provider and returns to `/trade/` — **not `/`** — after signing in.
2. Submitting a market order reaches the blotter and leaves `submitted`.
3. Cancelling shows "cancelling…" before it shows canceled.
4. A position with no live quote renders `—`, not `0`.
5. Double-clicking submit produces exactly one order.
6. With the `[auth.oidc]` block removed and the server restarted, `/trade/` returns 404 and `/cockpit/` still works.

- [ ] **Step 4: Commit**

```bash
git add readme.md docs/openapi.json
git commit -m "docs: the trade app, and how to enable it"
```

## Self-Review

**Spec coverage.** Placement and two entry points → Task 4. Asset base and shell serving → Task 3. `/trade/` gated on OIDC → Tasks 3, 4, 9. Login and `return_to` → Task 1. Separate API client → Task 4. Order ticket, idempotency, confirm, error table → Task 7. Blotter and the 202 cancel → Task 6. Positions and null marks → Task 8. Timeline reuse → Tasks 5, 6. Instrument endpoint → Task 2. Testing → every task, plus the checklist in Task 9.

**Gap found and closed while reviewing:** the spec says the two apps share components but not the client — yet `InstrumentSelect` and `OrderTimeline` both *import the cockpit client directly*. Parameterising only their paths would still have sent the admin token from the trade app, defeating the separate client entirely. Task 5 therefore makes the fetch function a prop as well as the path.

**Type consistency.** `Me` and `GrantedPortfolio` are defined in Task 4 and used in 6, 7 and 8. `tradeApi` is defined in Task 4 and used in 5–8. `search_instruments` is defined in Task 2 and consumed by `admin::list_instruments` in the same task. `respond(shell, path)` changes signature in Task 3, and every existing caller is updated in that same task.

**Known risk.** Task 3's tests assert that the two shells differ, which only holds once Task 4 has produced `trade.html`. Until then `respond("trade.html", …)` finds nothing and falls through to a 404. Run Task 3's new tests after Task 4's build, or guard them on `DIST.get_file("trade.html").is_some()` — the plan's ordering puts the Rust work first deliberately, so this is the one place where the sequence bites.
