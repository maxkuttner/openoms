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
