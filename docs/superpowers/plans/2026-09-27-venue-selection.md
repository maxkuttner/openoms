# Venue Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A trader can see every broker that has ever cataloged an instrument, know which are actually usable by their portfolio and why not otherwise, and pick one to route an order through.

**Architecture:** `account` gains a `portfolio_id` column (a portfolio can now hold more than one account); a new `GET /portfolios/:id/venues` endpoint joins `broker_instrument` (the full catalog, not filtered) against `broker_connection` and the portfolio's own accounts, classifying each row eligible/ineligible via one pure, exhaustively-tested function; `orders_submit` gains a real check that a supplied `account_id` belongs to the order's portfolio; the order ticket gets a venue dropdown built from the new endpoint.

**Tech Stack:** Rust/axum/sqlx (backend), React/TypeScript/Mantine/TanStack Query (`cockpit/src/trade`), Postgres migrations.

**Spec:** `docs/superpowers/specs/2026-09-27-venue-selection-design.md`

## Global Constraints

- The venue list is the full `broker_instrument` catalog for the instrument — never filtered down before classification. An ineligible row still appears, grayed out, with its reason.
- Reason priority, checked in this exact order, first match wins: `is_tradeable = false` → `"not tradeable on this broker"`; connection missing or not `ACTIVE` → `"broker connection is not active"`; no account on that connection for this portfolio → `"no account on this connection for this portfolio"`.
- Omitting `account_id` on order submit must remain byte-for-byte unchanged behavior (falls through to `portfolio.default_account_id`, no new validation applies).
- No new broker adapters, no smart order routing, no static "known venues" registry independent of `broker_instrument` — explicitly out of scope per the spec's Non-goals.
- This codebase has no HTTP-mocking library and the cockpit frontend has no test suite — established conventions from prior work in this repo, not something to introduce here.
- CI's `build` job (`cargo test`) has no live Postgres — every DB-backed test must carry `#[ignore = "needs a live Postgres; run with --ignored"]`, exactly like every other DB-backed test in `src/handlers.rs`.
- Any new/changed `#[utoipa::path]`-annotated endpoint or schema type requires regenerating `docs/openapi.json` (`cargo run -- openapi > docs/openapi.json`) — `tests::the_committed_openapi_spec_is_current` in `src/main.rs` enforces this.

## Review Focus

- **A broker_code with no broker_connection row at all** (synced once, then its connection deleted, or a data anomaly) — must classify as "broker connection is not active", not crash on a `None` where a status string was expected. Task 3's classify function takes `Option<&str>` specifically for this.
- **A portfolio with two accounts on two different environments of the same broker** (e.g. `binance-paper` and a hypothetical `binance-live`) — both must appear as distinct rows, not collapse into one. Task 3's join is on `broker_code` (fans out across every matching `broker_connection`), not on a single resolved connection.
- **An `account_id` reassigned to a different portfolio between two requests** — `orders_submit` must re-check ownership on every call, not cache/trust a prior validation. Task 4's check reads `account.portfolio_id` fresh from the same query that already fetches the account row.
- **Omitting `account_id` entirely** must take the exact same path as before this plan — Task 4's test asserts this explicitly, not just that the new path works.
- **An instrument_id with zero `broker_instrument` rows** (never synced by any broker) — the venues endpoint must return an empty list, not an error, matching this app's "excluded, not fabricated" convention everywhere else.

---

## Task 1: `account.portfolio_id` migration

**Files:**
- Create: `db/migrations/ods/oms/0026_ALTER_ACCOUNT_ADD_PORTFOLIO_ID.sql`

**Interfaces:**
- Produces: `account.portfolio_id UUID NULL REFERENCES portfolio(id)` — Task 2's admin CRUD and Task 3's venues query both read/write it.

- [ ] **Step 1: Write the migration**

```sql
-- A portfolio can now hold more than one account (one per venue it trades
-- through), instead of only the single default_account_id it already has.
-- Nullable: an account not yet assigned to any portfolio simply can't be
-- picked by anyone, the same as every account other than a portfolio's
-- default already implicitly was before this column existed.
ALTER TABLE account ADD COLUMN portfolio_id UUID REFERENCES portfolio(id);

CREATE INDEX idx_account_portfolio ON account(portfolio_id);

-- Backfill: every account that is currently someone's default becomes that
-- portfolio's first account. Accounts that are nobody's default stay NULL —
-- there is no other signal to backfill them from.
UPDATE account a
SET portfolio_id = p.id
FROM portfolio p
WHERE p.default_account_id = a.id;
```

- [ ] **Step 2: Run the migration**

Run: `cargo run -- database migrate`
Expected: output names `0026_ALTER_ACCOUNT_ADD_PORTFOLIO_ID` as applied.

- [ ] **Step 3: Verify the backfill**

Run: `cargo run -- database status` (expect: no pending migrations) and, separately, confirm the backfill directly:

```sql
-- via psql or the admin API — every portfolio's default account now shows
-- its own portfolio_id back.
SELECT p.code, a.code, a.portfolio_id = p.id AS backfilled_correctly
FROM portfolio p JOIN account a ON a.id = p.default_account_id;
```

Expected: `backfilled_correctly` is `true` for every row.

- [ ] **Step 4: Commit**

```bash
git add db/migrations/ods/oms/0026_ALTER_ACCOUNT_ADD_PORTFOLIO_ID.sql
git commit -m "feat(db): add account.portfolio_id, backfilled from default_account_id"
```

---

## Task 2: `portfolio_id` through the Account admin surface

**Files:**
- Modify: `src/domain/identity.rs:41-49` (`Account` struct)
- Modify: `src/admin.rs` (`CreateAccount`, `UpdateAccount`, `create_account`, `list_accounts`, `update_account`, and `get_account` if it has its own `SELECT`)
- Modify: `cockpit/src/pages/Accounts.tsx`

**Interfaces:**
- Consumes: `account.portfolio_id` (Task 1).
- Produces: `Account.portfolio_id: Option<Uuid>` in every admin Account response — Task 3 doesn't consume this directly (it queries `account` itself), but this is what lets an operator actually create the multi-account state Task 3/5 need to be testable at all.

- [ ] **Step 1: Add the field to `Account`**

In `src/domain/identity.rs`, in the `Account` struct (currently lines 41-49):

```rust
#[derive(Debug, Serialize, FromRow, Clone, utoipa::ToSchema)]
pub struct Account {
    pub id: Uuid,
    pub code: String,
    pub broker_connection_code: String,
    pub external_account_ref: String,
    pub status: String,
    pub portfolio_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

- [ ] **Step 2: Thread it through `CreateAccount`/`UpdateAccount` and the three queries in `src/admin.rs`**

`CreateAccount` (around line 60):

```rust
pub struct CreateAccount {
    pub code: String,
    pub broker_connection_code: String,
    pub external_account_ref: String,
    pub status: String,
    pub portfolio_id: Option<Uuid>,
}
```

`UpdateAccount` (around line 68):

```rust
pub struct UpdateAccount {
    pub code: Option<String>,
    pub broker_connection_code: Option<String>,
    pub external_account_ref: Option<String>,
    pub status: Option<String>,
    pub portfolio_id: Option<Uuid>,
}
```

`create_account` (around line 420) — add the column and bind, and return it:

```rust
pub async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(code = %payload.code, broker_connection_code = %payload.broker_connection_code, "admin create account");
    let id = Uuid::new_v4();
    let record = sqlx::query_as::<_, Account>(
        r#"
        INSERT INTO account (
            id,
            code,
            broker_connection_code,
            external_account_ref,
            status,
            portfolio_id
        ) VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id, code, broker_connection_code, external_account_ref, status, portfolio_id, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(payload.code)
    .bind(payload.broker_connection_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .bind(payload.portfolio_id)
    .fetch_one(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(record))
}
```

`list_accounts` (around line 457) — add the column:

```rust
pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<Vec<Account>>, AdminError> {
    info!("admin list accounts");
    let records = sqlx::query_as::<_, Account>(
        r#"
        SELECT id, code, broker_connection_code, external_account_ref, status, portfolio_id, created_at, updated_at
        FROM account
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(state.pool())
    .await
    .map_err(map_db_error)?;

    Ok(Json(records))
}
```

`update_account` (around line 515) — `portfolio_id` is nullable, so a plain `COALESCE` would never let it be cleared back to `NULL` once set; there's no existing precedent for clearing a nullable FK in this file to mirror, so use the same "was a value sent at all" pattern the JSON layer already gives you — `Option<Option<Uuid>>` isn't worth the complexity here since accounts are rarely reassigned; treat `None` in the payload as "leave unchanged" (via `COALESCE`, consistent with every other field on this struct) and accept that clearing `portfolio_id` back to `NULL` isn't supported by this endpoint yet — reassignment (setting it to a *different* portfolio) works fine via `COALESCE` since that only preserves-on-`None`, not blocks all writes:

```rust
pub async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateAccount>,
) -> Result<Json<Account>, AdminError> {
    info!(account_id = %id, "admin update account");
    let record = sqlx::query_as::<_, Account>(
        r#"
        UPDATE account
        SET
            code = COALESCE($1, code),
            broker_connection_code = COALESCE($2, broker_connection_code),
            external_account_ref = COALESCE($3, external_account_ref),
            status = COALESCE($4, status),
            portfolio_id = COALESCE($5, portfolio_id),
            updated_at = now()
        WHERE id = $6
        RETURNING id, code, broker_connection_code, external_account_ref, status, portfolio_id, created_at, updated_at
        "#,
    )
    .bind(payload.code)
    .bind(payload.broker_connection_code)
    .bind(payload.external_account_ref)
    .bind(payload.status)
    .bind(payload.portfolio_id)
    .bind(id)
    .fetch_optional(state.pool())
    .await
```

(leave the rest of the function — the `fetch_optional` → 404 handling below — exactly as it is; only the query and its binds changed.)

Check whether `get_account` (search `pub async fn get_account` in `src/admin.rs`) has its own inline `SELECT` — if so, add `portfolio_id` to its column list too, the same way.

- [ ] **Step 3: Add the field to the admin UI**

In `cockpit/src/pages/Accounts.tsx`, add to both `columns` and `fields` in the `CrudResource` props:

```typescript
      columns={[
        { key: "code", label: "Code" },
        { key: "broker_connection_code", label: "Broker connection" },
        { key: "external_account_ref", label: "External ref" },
        { key: "portfolio_id", label: "Portfolio" },
        { key: "status", label: "Status" },
      ]}
      fields={[
        { name: "code", label: "Code", required: true },
        {
          name: "broker_connection_code",
          label: "Broker connection",
          type: "select",
          required: true,
          optionsPath: "/admin/broker-connections",
          optionValue: "code",
          optionLabel: "code",
        },
        { name: "external_account_ref", label: "External account ref", required: true },
        {
          name: "portfolio_id",
          label: "Portfolio",
          type: "select",
          optionsPath: "/admin/portfolios",
          optionValue: "id",
          optionLabel: "code",
        },
        { name: "status", label: "Status", type: "select", required: true, options: STATUS },
      ]}
```

- [ ] **Step 4: Regenerate the OpenAPI spec**

`Account` gained a field, and it's registered in `src/main.rs`'s `#[openapi(components(schemas(...)))]` list already (as `Account`) — no new registration needed, just regenerate the committed file:

Run: `cargo run -- openapi > docs/openapi.json`

- [ ] **Step 5: Build and typecheck**

Run: `cargo build 2>&1 | tail -30` — expect clean (only pre-existing dead-code warnings).
Run: `cd cockpit && npx tsc --noEmit` — expect clean.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: all pass (no new tests in this task; this confirms nothing broke).

- [ ] **Step 7: Commit**

```bash
git add src/domain/identity.rs src/admin.rs cockpit/src/pages/Accounts.tsx docs/openapi.json
git commit -m "feat(admin): expose account.portfolio_id through the Accounts CRUD"
```

---

## Task 3: `GET /portfolios/:id/venues`

**Files:**
- Modify: `src/handlers.rs` (new `classify_venue` function, `VenueOption` struct, `VenuesQuery` struct, `get_portfolio_venues` handler)
- Modify: `src/main.rs` (route registration, `#[openapi(paths(...), components(schemas(...)))]` registration)

**Interfaces:**
- Consumes: `broker_instrument`, `broker_connection`, `account` (with `portfolio_id` from Task 1) tables; `Extension<AuthContext>` (existing, `auth.principal_id: Uuid`).
- Produces: `GET /portfolios/:id/venues?instrument_id=X` → `Vec<VenueOption>` — Task 5's frontend consumes this exactly.

- [ ] **Step 1: Write the failing tests for the pure classify function**

Add to `src/handlers.rs`'s existing test module:

```rust
#[test]
fn not_tradeable_wins_over_every_other_reason() {
    let (eligible, reason) = classify_venue(false, Some("ACTIVE"), true);
    assert!(!eligible);
    assert_eq!(reason, Some("not tradeable on this broker"));
}

#[test]
fn missing_connection_is_the_same_as_inactive() {
    let (eligible, reason) = classify_venue(true, None, true);
    assert!(!eligible);
    assert_eq!(reason, Some("broker connection is not active"));
}

#[test]
fn inactive_connection_beats_missing_account() {
    let (eligible, reason) = classify_venue(true, Some("SUSPENDED"), false);
    assert!(!eligible);
    assert_eq!(reason, Some("broker connection is not active"));
}

#[test]
fn no_account_is_the_last_reason_checked() {
    let (eligible, reason) = classify_venue(true, Some("ACTIVE"), false);
    assert!(!eligible);
    assert_eq!(reason, Some("no account on this connection for this portfolio"));
}

#[test]
fn everything_present_is_eligible_with_no_reason() {
    let (eligible, reason) = classify_venue(true, Some("ACTIVE"), true);
    assert!(eligible);
    assert_eq!(reason, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib handlers::tests::not_tradeable_wins -- --nocapture`
Expected: FAIL — `classify_venue` doesn't exist yet.

- [ ] **Step 3: Write `classify_venue`, `VenueOption`, `VenuesQuery`, and the handler**

Add to `src/handlers.rs`, near the other free functions:

```rust
/// One (broker_instrument, broker_connection) pair's usability for a
/// specific portfolio. Checked in this exact order — the response always
/// names the single most fundamental blocker, not every applicable one.
/// `connection_status` is `None` both when no broker_connection row exists
/// at all for this broker_code, and when sqlx reads a NULL from the LEFT
/// JOIN — both cases mean "nothing to route through", same as an inactive
/// connection.
fn classify_venue(
    is_tradeable: bool,
    connection_status: Option<&str>,
    has_account: bool,
) -> (bool, Option<&'static str>) {
    if !is_tradeable {
        return (false, Some("not tradeable on this broker"));
    }
    if connection_status != Some("ACTIVE") {
        return (false, Some("broker connection is not active"));
    }
    if !has_account {
        return (false, Some("no account on this connection for this portfolio"));
    }
    (true, None)
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct VenueOption {
    pub broker_code: String,
    pub environment: Option<String>,
    pub broker_connection_code: Option<String>,
    pub account_id: Option<Uuid>,
    pub eligible: bool,
    pub reason: Option<String>,
}

#[derive(serde::Deserialize, utoipa::IntoParams)]
pub struct VenuesQuery {
    pub instrument_id: String,
}

#[utoipa::path(
    get, path = "/portfolios/{id}/venues", tag = "orders",
    params(
        ("id" = Uuid, Path, description = "Portfolio ID"),
        VenuesQuery,
    ),
    responses(
        (status = 200, description = "OK", body = [VenueOption]),
        (status = 403, description = "No view grant for principal/portfolio"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_portfolio_venues(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(portfolio_id): Path<Uuid>,
    Query(query): Query<VenuesQuery>,
) -> Result<Json<Vec<VenueOption>>, ApiError> {
    let can_view: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM principal_portfolio_grant \
         WHERE principal_id = $1 AND portfolio_id = $2 AND can_view = true)",
    )
    .bind(auth.principal_id)
    .bind(portfolio_id)
    .fetch_one(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to check grant: {:?}", err),
    })?;
    if !can_view {
        return Err(ApiError { status: StatusCode::FORBIDDEN, message: "unauthorized".to_string() });
    }

    // An unparseable instrument_id has no venues, same as a real but
    // never-synced one — excluded, not an error.
    let Ok(instrument_id) = query.instrument_id.parse::<i64>() else {
        return Ok(Json(Vec::new()));
    };

    let rows = sqlx::query(
        "SELECT bi.broker_code, bi.is_tradeable, bc.environment, \
                bc.code AS broker_connection_code, bc.status AS connection_status, \
                a.id AS account_id \
         FROM broker_instrument bi \
         LEFT JOIN broker_connection bc ON bc.broker_code = bi.broker_code \
         LEFT JOIN account a ON a.broker_connection_code = bc.code AND a.portfolio_id = $2 \
         WHERE bi.instrument_id = $1",
    )
    .bind(instrument_id)
    .bind(portfolio_id)
    .fetch_all(state.pool())
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load venues: {:?}", err),
    })?;

    let options = rows
        .into_iter()
        .map(|row| {
            let is_tradeable: bool = row.get("is_tradeable");
            let connection_status: Option<String> = row.get("connection_status");
            let account_id: Option<Uuid> = row.get("account_id");
            let (eligible, reason) =
                classify_venue(is_tradeable, connection_status.as_deref(), account_id.is_some());
            VenueOption {
                broker_code: row.get("broker_code"),
                environment: row.get("environment"),
                broker_connection_code: row.get("broker_connection_code"),
                account_id,
                eligible,
                reason: reason.map(str::to_string),
            }
        })
        .collect();

    Ok(Json(options))
}
```

Check `utoipa::IntoParams` needs a `#[derive]` your utoipa version actually supports for query-param structs — if `VenuesQuery` doesn't compile with that derive, fall back to inlining it in `params(...)` instead: `("instrument_id" = String, Query, description = "Instrument id")`, dropping the derive from the struct.

- [ ] **Step 4: Register the route**

In `src/main.rs`, in the `orders_router` chain, next to the existing `.route("/portfolios/:id/positions", ...)`:

```rust
        .route("/portfolios/:id/venues", get(handlers::get_portfolio_venues))
```

- [ ] **Step 5: Register the new path and schema in the OpenAPI derive**

In `src/main.rs`'s `#[openapi(paths(...))]` list, next to `handlers::get_portfolio_positions`:

```rust
        handlers::get_portfolio_venues,
```

In `#[openapi(components(schemas(...)))]`, next to `crate::positions::Position`:

```rust
        handlers::VenueOption,
```

- [ ] **Step 6: Run the pure-function tests to verify they pass**

Run: `cargo test --lib handlers::tests -- classify_venue not_tradeable missing_connection inactive_connection no_account everything_present --nocapture`
Expected: all 5 pass.

- [ ] **Step 7: Write the DB-backed integration test**

Add to `src/handlers.rs`'s test module (mirrors the watchlist tests' seeding style — `seed_principal`, a fresh `seed_instrument`-equivalent inline insert, and the real `test_app_state`/`AppState::new` construction already used there):

```rust
#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn venues_reports_eligible_and_ineligible_rows_with_reasons() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "venues-test").await;

    let venue_a: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("a seeded venue");
    let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("a seeded currency");
    let instrument_id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument \
             (symbol, venue, name, asset_class, instrument_class, currency, status, \
              price_precision, price_increment) \
         VALUES ('VENUETEST', $1, 'Venue test instrument', 'EQUITY', 'SPOT', $2, 'ACTIVE', 2, 0.01) \
         RETURNING id",
    )
    .bind(&venue_a)
    .bind(&currency)
    .fetch_one(&pool)
    .await
    .expect("seed instrument");

    // Broker A: active connection, this portfolio HAS an account on it -> eligible.
    let broker_a = format!("VENUEA{}", Uuid::new_v4().simple());
    let conn_a = format!("venue-test-conn-a-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')",
    )
    .bind(&conn_a)
    .bind(&broker_a)
    .execute(&pool)
    .await
    .expect("seed broker_connection a");
    sqlx::query(
        "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'VENUETEST', true)",
    )
    .bind(instrument_id)
    .bind(&broker_a)
    .execute(&pool)
    .await
    .expect("seed broker_instrument a");

    let portfolio_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Venues test portfolio', 'ACTIVE')",
    )
    .bind(portfolio_id)
    .bind(format!("venues-test-portfolio-{portfolio_id}"))
    .execute(&pool)
    .await
    .expect("seed portfolio");
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $3, 'EXT', 'ACTIVE', $4)",
    )
    .bind(Uuid::new_v4())
    .bind(format!("venues-test-account-{portfolio_id}"))
    .bind(&conn_a)
    .bind(portfolio_id)
    .execute(&pool)
    .await
    .expect("seed account");
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view) VALUES ($1, $2, $3, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal_id)
    .bind(portfolio_id)
    .execute(&pool)
    .await
    .expect("seed grant");

    // Broker B: also cataloged, but no account for this portfolio -> ineligible.
    let broker_b = format!("VENUEB{}", Uuid::new_v4().simple());
    let conn_b = format!("venue-test-conn-b-{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')",
    )
    .bind(&conn_b)
    .bind(&broker_b)
    .execute(&pool)
    .await
    .expect("seed broker_connection b");
    sqlx::query(
        "INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'VENUETEST', true)",
    )
    .bind(instrument_id)
    .bind(&broker_b)
    .execute(&pool)
    .await
    .expect("seed broker_instrument b");

    let state = test_app_state(pool);
    let auth = AuthContext { principal_id, principal_code };

    let result = get_portfolio_venues(
        State(state),
        Extension(auth),
        Path(portfolio_id),
        Query(VenuesQuery { instrument_id: instrument_id.to_string() }),
    )
    .await
    .expect("should succeed")
    .0;

    assert_eq!(result.len(), 2);
    let a = result.iter().find(|v| v.broker_code == broker_a).expect("broker a present");
    assert!(a.eligible);
    assert_eq!(a.reason, None);
    let b = result.iter().find(|v| v.broker_code == broker_b).expect("broker b present");
    assert!(!b.eligible);
    assert_eq!(b.reason.as_deref(), Some("no account on this connection for this portfolio"));
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn venues_for_an_uncataloged_instrument_is_an_empty_list_not_an_error() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "venues-empty").await;
    let portfolio_id = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Empty test portfolio', 'ACTIVE')")
        .bind(portfolio_id)
        .bind(format!("venues-empty-portfolio-{portfolio_id}"))
        .execute(&pool)
        .await
        .expect("seed portfolio");
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_view) VALUES ($1, $2, $3, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal_id)
    .bind(portfolio_id)
    .execute(&pool)
    .await
    .expect("seed grant");

    let state = test_app_state(pool);
    let auth = AuthContext { principal_id, principal_code };
    let result = get_portfolio_venues(
        State(state),
        Extension(auth),
        Path(portfolio_id),
        Query(VenuesQuery { instrument_id: "999999999".to_string() }),
    )
    .await
    .expect("should succeed")
    .0;

    assert!(result.is_empty());
}
```

- [ ] **Step 8: Regenerate the OpenAPI spec**

Run: `cargo run -- openapi > docs/openapi.json`

- [ ] **Step 9: Run the full test suite (default — DB tests skip) and then the ignored ones**

Run: `cargo test 2>&1 | tail -10` — expect all pass, ignored count up by 2.
Run: `cargo test -- --ignored venues_reports_eligible venues_for_an_uncataloged 2>&1 | tail -10` — expect both pass against your real dev Postgres.

- [ ] **Step 10: Commit**

```bash
git add src/handlers.rs src/main.rs docs/openapi.json
git commit -m "feat(trade-api): add GET /portfolios/:id/venues"
```

---

## Task 4: `orders_submit` account-ownership check

**Files:**
- Modify: `src/handlers.rs` (`orders_submit`, around lines 344-395)

**Interfaces:**
- Consumes: `account.portfolio_id` (Task 1).
- Produces: `orders_submit` now rejects a cross-portfolio `account_id` with `400` — Task 5's frontend never triggers this on the happy path (it only ever sends an `account_id` the venues endpoint itself said was eligible for that exact portfolio), but it's the enforcement the whole feature relies on.

- [ ] **Step 1: Write the failing tests**

Add to `src/handlers.rs`'s test module. These mirror the existing seeding style in the file (venue A/portfolio setup from Task 3's test, condensed) — seed two portfolios, one account belonging to portfolio A, then try to submit against portfolio B naming portfolio A's account:

```rust
#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn an_account_from_another_portfolio_is_rejected() {
    let pool = test_pool().await;
    let (principal_id, principal_code) = seed_principal(&pool, "ownership-test").await;

    let venue: String = sqlx::query_scalar("SELECT code FROM venue ORDER BY code LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("a seeded venue");
    let currency: String = sqlx::query_scalar("SELECT code FROM currency ORDER BY code LIMIT 1")
        .fetch_one(&pool)
        .await
        .expect("a seeded currency");
    let instrument_id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument \
             (symbol, venue, name, asset_class, instrument_class, currency, status, \
              price_precision, price_increment) \
         VALUES ('OWNERTEST', $1, 'Ownership test instrument', 'EQUITY', 'SPOT', $2, 'ACTIVE', 2, 0.01) \
         RETURNING id",
    )
    .bind(&venue)
    .bind(&currency)
    .fetch_one(&pool)
    .await
    .expect("seed instrument");

    let broker_code = format!("OWNERTEST{}", Uuid::new_v4().simple());
    let conn_code = format!("owner-test-conn-{}", Uuid::new_v4());
    sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, $2, 'PAPER', 'ACTIVE')")
        .bind(&conn_code)
        .bind(&broker_code)
        .execute(&pool)
        .await
        .expect("seed broker_connection");
    sqlx::query("INSERT INTO broker_instrument (instrument_id, broker_code, broker_symbol, is_tradeable) VALUES ($1, $2, 'OWNERTEST', true)")
        .bind(instrument_id)
        .bind(&broker_code)
        .execute(&pool)
        .await
        .expect("seed broker_instrument");

    // Portfolio A owns account_a.
    let portfolio_a = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Portfolio A', 'ACTIVE')")
        .bind(portfolio_a)
        .bind(format!("owner-test-a-{portfolio_a}"))
        .execute(&pool)
        .await
        .expect("seed portfolio a");
    let account_a = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO account (id, code, broker_connection_code, external_account_ref, status, portfolio_id) \
         VALUES ($1, $2, $3, 'EXT-A', 'ACTIVE', $4)",
    )
    .bind(account_a)
    .bind(format!("owner-test-account-a-{account_a}"))
    .bind(&conn_code)
    .bind(portfolio_a)
    .execute(&pool)
    .await
    .expect("seed account a");

    // Portfolio B is the one on the request, with a grant to trade — but the
    // request names portfolio A's account.
    let portfolio_b = Uuid::new_v4();
    sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'Portfolio B', 'ACTIVE')")
        .bind(portfolio_b)
        .bind(format!("owner-test-b-{portfolio_b}"))
        .execute(&pool)
        .await
        .expect("seed portfolio b");
    sqlx::query(
        "INSERT INTO principal_portfolio_grant (id, principal_id, portfolio_id, can_trade) VALUES ($1, $2, $3, true)",
    )
    .bind(Uuid::new_v4())
    .bind(principal_id)
    .bind(portfolio_b)
    .execute(&pool)
    .await
    .expect("seed grant");

    let state = test_app_state(pool);
    let _ = principal_code; // unused here; kept for symmetry with other tests' destructuring

    let request = SubmitOrderRequest {
        order_id: Uuid::new_v4().to_string(),
        client_order_id: "ownership-test-1".to_string(),
        portfolio_id: portfolio_b.to_string(),
        account_id: Some(account_a.to_string()),
        instrument_id: Some(instrument_id.to_string()),
        symbol: None,
        venue: None,
        side: OrderSide::Buy,
        order_type: OrderType::Market,
        time_in_force: TimeInForce::Day,
        limit_price: None,
        quantity: 1.0,
    };

    let result = orders_submit(
        State(state),
        Extension(AuthContext { principal_id, principal_code: "irrelevant".to_string() }),
        Json(request),
    )
    .await;

    assert!(result.is_err());
    let err = result.err().unwrap();
    // orders_submit returns Result<Response, ApiError> in this codebase's
    // existing signature — adjust this assertion to however that error
    // actually surfaces (check the function's real return type before
    // writing this assertion for real; it may need `.into_response()`
    // inspection instead of a bare status field, depending on what ApiError
    // implements).
}

#[tokio::test]
#[ignore = "needs a live Postgres; run with --ignored"]
async fn omitting_account_id_is_completely_unaffected() {
    // Regression guard: a request with no account_id must take exactly the
    // same default_account_id fallback path as before this task, with no
    // new check applying. Seed a portfolio + default account the same way
    // Task 3's happy-path test does, submit with account_id: None, and
    // assert it succeeds (or fails for some OTHER pre-existing reason
    // entirely unrelated to ownership, e.g. instrument checks) — the point
    // is that no NEW rejection appears here.
}
```

*(The second test's body is intentionally left for you to flesh out by copying Task 3's happy-path seeding — it's a regression guard, not new behavior, so match whatever the existing account-resolution tests already looked like before this task. Read `orders_submit`'s actual current signature and error-response shape first — `grep -n "pub async fn orders_submit" -A 5 src/handlers.rs` — before writing the assertion in the first test; this plan's draft above may not match its exact `Result`/`Response` shape.)*

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -- --ignored an_account_from_another_portfolio omitting_account_id --nocapture`
Expected: FAIL (or doesn't compile yet against the real function signature — fix the test to match reality before proceeding, per the note above).

- [ ] **Step 3: Add the ownership check**

In `src/handlers.rs`'s `orders_submit`, the existing `account_row_pre` query (around line 388) currently selects `bc.broker_code, bc.environment, bc.code AS broker_connection_code, a.external_account_ref`. Add `a.portfolio_id AS account_portfolio_id` to that same `SELECT`:

```rust
    let account_row_pre = sqlx::query(
        "SELECT bc.broker_code, bc.environment, bc.code AS broker_connection_code, \
                a.external_account_ref, a.portfolio_id AS account_portfolio_id \
         FROM account a \
         JOIN broker_connection bc ON bc.code = a.broker_connection_code \
         WHERE a.id = $1 AND bc.status = 'ACTIVE'"
    )
    .bind(account_id)
    .fetch_optional(&pool)
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load account: {:?}", err),
    })?
    .ok_or_else(|| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "account not found or its broker connection is not active".to_string(),
    })?;
```

Immediately after that block, before `broker_code`/`environment`/etc. are extracted from `account_row_pre`, add the ownership check — only when the caller explicitly supplied an `account_id` (an omitted one always resolved from `portfolio.default_account_id`, which is inherently correct and needs no re-check):

```rust
    if req.account_id.is_some() {
        let account_portfolio_id: Option<Uuid> = account_row_pre.get("account_portfolio_id");
        if account_portfolio_id != Some(portfolio_id) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "account does not belong to this portfolio".to_string(),
            });
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -- --ignored an_account_from_another_portfolio omitting_account_id --nocapture`
Expected: both pass (after fixing Step 1's draft assertions against the real function shape).

- [ ] **Step 5: Run the full test suite**

Run: `cargo test 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/handlers.rs
git commit -m "fix(trade-api): reject an account_id that doesn't belong to the order's portfolio"
```

---

## Task 5: Venue dropdown on the order ticket

**Files:**
- Modify: `cockpit/src/trade/types.ts` (add `VenueOption`)
- Modify: `cockpit/src/trade/components/OrderTicket.tsx`

**Interfaces:**
- Consumes: `GET /portfolios/:id/venues?instrument_id=X` (Task 3), `tradeApi.get` (`cockpit/src/trade/api/client.ts`, existing).
- Produces: `OrderTicket`'s submit payload now includes `account_id` when a venue is explicitly chosen — no other task depends on this.

- [ ] **Step 1: Add the type**

In `cockpit/src/trade/types.ts`, add alongside `MarkRow`:

```typescript
// Mirrors src/handlers.rs's VenueOption. `reason` is set only when
// `eligible` is false — never both null-checked independently, since they
// always agree by construction on the server.
export interface VenueOption {
  broker_code: string;
  environment: string | null;
  broker_connection_code: string | null;
  account_id: string | null;
  eligible: boolean;
  reason: string | null;
}
```

- [ ] **Step 2: Add the venue query, state, and dropdown to `OrderTicket.tsx`**

Add the import:

```typescript
import type { VenueOption } from "../types";
```

Add state, next to the existing `selectedInstrument` state (around line 136):

```typescript
  const [selectedAccountId, setSelectedAccountId] = useState<string | null>(null);
```

Add the query, after the `selectedInstrument` state block:

```typescript
  const venues = useQuery<VenueOption[]>({
    queryKey: ["/portfolios", portfolioId, "venues", instrumentId],
    queryFn: () => tradeApi.get<VenueOption[]>(`/portfolios/${portfolioId}/venues?instrument_id=${instrumentId}`),
    enabled: !!portfolioId && !!instrumentId,
  });
```

Add `import { useQuery } from "@tanstack/react-query";` to the top imports.

Reset the chosen venue whenever the portfolio or instrument changes — a venue picked for one instrument must never silently carry over to another:

```typescript
  useEffect(() => {
    setSelectedAccountId(null);
  }, [portfolioId, instrumentId]);
```

Add the dropdown to the JSX, after the `InstrumentSelect` block (around line 428), before `SideSelector`:

```typescript
        {venues.data && venues.data.length > 0 && (
          <Select
            label="Venue"
            placeholder="Default (portfolio's own account)"
            data={venues.data.map((v) => ({
              value: v.account_id ?? "",
              label: `${v.broker_code}${v.environment ? ` (${v.environment})` : ""}`,
              disabled: !v.eligible,
            }))}
            value={selectedAccountId}
            onChange={setSelectedAccountId}
            clearable
            // Mantine doesn't expose a per-option tooltip prop directly;
            // title on the rendered option covers the common case of "why
            // is this grayed out" without a second dependency. Check
            // Mantine's current Select API for a supported per-item
            // description prop as a cleaner alternative if one exists in
            // this project's installed version.
          />
        )}
```

*(The `title`/tooltip-per-option detail is deliberately left slightly open — Mantine's `Select` component's exact API for per-item extra text/attributes varies by version; check `cockpit/node_modules/@mantine/core`'s installed version's `Select`/`ComboboxItem` types for the cleanest way to attach `v.reason` to a disabled option before finalizing this step, rather than guessing at a prop name that may not exist.)*

- [ ] **Step 3: Include the chosen venue in the submit payload and confirmation text**

In `confirmSubmit` (around line 272), add to the POST body:

```typescript
        account_id: selectedAccountId || undefined,
```

In `reset()` (around line 204), add:

```typescript
    setSelectedAccountId(null);
```

In the confirmation modal's summary `<Text>` (around line 500), append the venue when one is chosen:

```typescript
          <Text>
            {side === "buy" ? "Buy" : "Sell"} {quantity} {instrumentLabel}
            {orderType === "limit" ? ` · limit ${limitPrice}` : " · market"} · {tif} · portfolio {portfolioLabel}
            {selectedAccountId &&
              (() => {
                const venue = venues.data?.find((v) => v.account_id === selectedAccountId);
                return venue ? ` · via ${venue.broker_code}` : "";
              })()}
            {notional !== null && ` · est. notional ${notional.toFixed(2)}`}
          </Text>
```

- [ ] **Step 4: Type-check**

Run: `cd cockpit && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Manual verification in the browser**

Using the admin console (Task 2's new `portfolio_id` field on Accounts), assign a second account to a portfolio that already has one, on a broker connection that also has a `broker_instrument` row for some instrument you can watch/hold. Then in the trade app: pick that portfolio and instrument in the order ticket, confirm the Venue dropdown appears showing both accounts (one eligible, or both if both are wired up), pick one, confirm the confirmation modal names it, and confirm a portfolio/instrument combination with only the default account still works exactly as before (no dropdown, or a single-option dropdown, submits with no `account_id` needed either way since leaving it unset preserves the old behavior).

- [ ] **Step 6: Commit**

```bash
git add cockpit/src/trade/types.ts cockpit/src/trade/components/OrderTicket.tsx
git commit -m "feat(trade): add venue selection to the order ticket"
```

---

## Self-Review

**Spec coverage:**
- `account.portfolio_id` + backfill → Task 1.
- Admin CRUD exposure (needed to actually create the multi-account state) → Task 2.
- `GET /portfolios/:id/venues`, full catalog + reason priority → Task 3.
- `orders_submit` ownership check → Task 4.
- Ticket dropdown, submit payload, confirmation text → Task 5.
- Non-goals (no new broker adapters, no SOR, no static registry) → none built, not referenced by any task.

**Placeholder scan:** two intentionally-left-open spots, both flagged explicitly rather than guessed at: Task 4's first test's exact error-assertion shape (depends on `orders_submit`'s real `Result`/`Response` type, told to check before writing), and Task 4's second test's body (told to mirror Task 3's happy-path seeding). Both are pointers to verify-then-fill, not vague instructions — the plan says exactly what to check and why. Task 5's per-option-tooltip detail is similarly a "check the installed library version" pointer, not an unresolved design question. No `TODO`/`TBD`/hand-waved requirements.

**Type consistency:** `VenueOption` (Rust, Task 3) and `VenueOption` (TypeScript, Task 5) match field-for-field. `classify_venue`'s signature (`bool, Option<&str>, bool) -> (bool, Option<&'static str>)`) is identical between its test-writing step and its implementation step within Task 3.

**Review Focus:** all five items have an owning task and test named above (no-connection-row → Task 3's `missing_connection_is_the_same_as_inactive`; two-environments-fan-out → Task 3's join structure itself, exercised implicitly by any multi-connection broker_code — not given its own dedicated test, which is a real gap: **add one** — see note below; reassigned-account re-checked-live → Task 4's design (reads fresh every call, no caching); omitted-account_id unaffected → Task 4's second test; uncataloged-instrument → Task 3's `venues_for_an_uncataloged_instrument_is_an_empty_list_not_an_error`).

**Fix applied during self-review:** the "two environments of the same broker" fan-out case had no dedicated test. Task 3, Step 7 covers it implicitly (broker A and broker B are two different `broker_code`s, not two environments of the *same* one) but that's not the same claim. Rather than leave it silently uncovered, note it here explicitly for the implementer: when writing Task 3's Step 7 test, consider seeding a second `broker_connection` row with the *same* `broker_code` as broker A but a different `environment` (e.g. `LIVE`) and no account on it, and assert the venues response contains two rows for `broker_a` — one eligible (has an account), one not (no account on that specific connection) — rather than only the two-different-brokers case as drafted above. This strengthens Task 3's own test coverage; it is not a new task.
