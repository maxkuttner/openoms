//! Instrument catalog lookup.
//!
//! The catalog is reference data — the same symbols, venues and names for every
//! principal — so the trader endpoint applies no grant filtering. It exists
//! separately from the admin one only so an order ticket can search without an
//! admin token.

use axum::{
    extract::{Query, State},
    Json,
};
use sqlx::PgPool;

use crate::admin::{InstrumentSearch, InstrumentSummary};
use crate::app_state::AppState;
use crate::handlers::ApiError;

/// Default to 50, clamp into `[1, 200]` — a zero, negative or absent limit
/// floors to 1 rather than erroring or running unbounded; an oversized one
/// caps at 200. Pulled out as a pure function so the clamp itself can be
/// tested without a database.
fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 200)
}

/// The one query behind both the admin and trader endpoints.
pub async fn search_instruments(
    pool: &PgPool,
    search: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<InstrumentSummary>, sqlx::Error> {
    let pattern = search.map(|s| format!("%{s}%"));
    let limit = clamp_limit(limit);

    sqlx::query_as::<_, InstrumentSummary>(
        "SELECT i.id, i.symbol, i.name, i.venue, i.asset_class, i.instrument_class, i.status, \
                EXISTS (SELECT 1 FROM public.instrument_derivative d \
                        WHERE d.underlying_id = i.id OR d.underlying_symbol = i.symbol) AS has_options \
         FROM public.instrument i \
         WHERE i.status = 'ACTIVE' AND ($1::text IS NULL OR i.symbol ILIKE $1 OR i.name ILIKE $1) \
         ORDER BY i.symbol \
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

    /// No database: exercises `clamp_limit` directly so the clamp itself is
    /// pinned, independent of what any particular search happens to match.
    /// A vacuous search-based assertion (previously used here) would pass
    /// even with `.clamp()` deleted entirely — see fix-round-1 report.
    #[test]
    fn the_limit_is_clamped_to_the_documented_range() {
        assert_eq!(clamp_limit(None), 50, "default is 50");
        assert_eq!(clamp_limit(Some(0)), 1, "zero floors to 1");
        assert_eq!(clamp_limit(Some(-5)), 1, "negative floors to 1");
        assert_eq!(clamp_limit(Some(10_000)), 200, "oversized caps at 200");
    }

    /// Proves the clamped value actually reaches the SQL `LIMIT`, not just
    /// that `clamp_limit` computes the right number in isolation.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_clamped_limit_actually_bounds_the_query() {
        let pool = test_pool().await;
        let prefix = format!("ZZLIMIT{}", &uuid::Uuid::new_v4().to_string()[..8]);
        for i in 0..3 {
            seed_instrument(&pool, &format!("{prefix}{i}"), "ACTIVE").await;
        }

        let rows = search_instruments(&pool, Some(&prefix), Some(2)).await.expect("search");
        assert_eq!(rows.len(), 2, "LIMIT 2 must return exactly 2 of the 3 matching rows");
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

    /// Gives `symbol` (already seeded via `seed_instrument`) one option leg as
    /// its underlying — enough for `has_options` to flip true. The option leg
    /// itself doesn't need to be a valid tradeable contract, just a row in
    /// `instrument_derivative` naming this symbol.
    async fn seed_option_leg(pool: &sqlx::PgPool, underlying_symbol: &str) -> i64 {
        let leg_id = seed_instrument(pool, &format!("{underlying_symbol}LEG"), "ACTIVE").await;
        sqlx::query(
            "INSERT INTO public.instrument_derivative \
                (instrument_id, underlying_symbol, option_kind, strike_price, expiry_date) \
             VALUES ($1, $2, 'CALL', 100.0, CURRENT_DATE + 30)",
        )
        .bind(leg_id)
        .bind(underlying_symbol)
        .execute(pool)
        .await
        .expect("seed instrument_derivative");
        leg_id
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn has_options_is_true_only_for_an_instrument_with_a_derivative_row() {
        let pool = test_pool().await;
        let with_options = seed_instrument(&pool, "ZZHASOPT", "ACTIVE").await;
        let without_options = seed_instrument(&pool, "ZZNOOPT", "ACTIVE").await;

        let with_symbol: String =
            sqlx::query_scalar("SELECT symbol FROM public.instrument WHERE id = $1")
                .bind(with_options)
                .fetch_one(&pool)
                .await
                .expect("symbol");
        seed_option_leg(&pool, &with_symbol).await;

        let with_rows = search_instruments(&pool, Some(&with_symbol), None).await.expect("search");
        let with_row = with_rows.iter().find(|r| r.id == with_options).expect("found");
        assert!(with_row.has_options, "an instrument with a derivative row must show has_options");
        assert_eq!(with_row.instrument_class, "SPOT");

        let without_symbol: String =
            sqlx::query_scalar("SELECT symbol FROM public.instrument WHERE id = $1")
                .bind(without_options)
                .fetch_one(&pool)
                .await
                .expect("symbol");
        let without_rows =
            search_instruments(&pool, Some(&without_symbol), None).await.expect("search");
        let without_row = without_rows.iter().find(|r| r.id == without_options).expect("found");
        assert!(!without_row.has_options, "an instrument with no derivative row must not");
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
    ///
    /// The brief's original INSERT assumed `(symbol, name, venue, asset_class,
    /// instrument_class, status)` was the full NOT-NULL set and used
    /// `instrument_class = 'STOCK'`. Neither holds: `db/migrations/ods/public/
    /// 0003_CREATE_INSTRUMENT_TABLE.sql` also requires `currency`,
    /// `price_precision` and `price_increment` (NOT NULL, no default), and its
    /// CHECK constraint only allows `instrument_class` to be one of
    /// SPOT/FUTURE/FORWARD/OPTION/SWAP/CFD/BOND/WARRANT — 'STOCK' would violate
    /// it. `venue` and `currency` are also FKs, so rather than hard-coding a
    /// code that may not be seeded in every environment, this pulls whatever the
    /// database already has — the same approach `src/handlers.rs`'s actor
    /// attribution test uses.
    async fn seed_instrument(pool: &sqlx::PgPool, symbol: &str, status: &str) -> i64 {
        let unique = format!("{symbol}{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let venue: String = sqlx::query_scalar("SELECT code FROM public.venue ORDER BY code LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("a seeded venue");
        let currency: String = sqlx::query_scalar("SELECT code FROM public.currency ORDER BY code LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("a seeded currency");

        sqlx::query_scalar::<_, i64>(
            "INSERT INTO public.instrument \
                 (symbol, name, venue, asset_class, instrument_class, currency, status, \
                  price_precision, price_increment) \
             VALUES ($1, $2, $3, 'EQUITY', 'SPOT', $4, $5, 2, 0.01) RETURNING id",
        )
        .bind(&unique)
        .bind(format!("Test {unique}"))
        .bind(&venue)
        .bind(&currency)
        .bind(status)
        .fetch_one(pool)
        .await
        .expect("seed instrument")
    }
}
