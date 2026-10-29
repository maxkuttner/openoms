//! Instrument catalog lookup.
//!
//! The catalog is reference data — the same symbols, venues and names for every
//! principal — so the trader endpoint applies no grant filtering. It exists
//! separately from the admin one only so an order ticket can search without an
//! admin token.

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    Json,
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::admin::{InstrumentSearch, InstrumentSummary};
use crate::app_state::AppState;
use crate::auth::AuthContext;
use crate::handlers::ApiError;

/// Default to 50, clamp into `[1, 200]` — a zero, negative or absent limit
/// floors to 1 rather than erroring or running unbounded; an oversized one
/// caps at 200. Pulled out as a pure function so the clamp itself can be
/// tested without a database.
fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(50).clamp(1, 200)
}

/// The query as a literal inside an ILIKE pattern: `%`, `_` and the escape
/// character itself would otherwise act as wildcards.
fn escape_like(search: &str) -> String {
    search.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// The one query behind both the admin and trader endpoints.
///
/// Ranked the way trading terminals rank a symbol search, strongest first:
/// 1. match quality — exact symbol, symbol prefix, a word of the name
///    starting with the query, then plain substring;
/// 2. the searching principal's own instruments (watched, held in a
///    portfolio it may view, or ordered) — `None` for the admin catalog;
/// 3. option contracts after everything else, so a popular underlying's
///    thousands of contracts never crowd its stock/ETF matches off the page;
/// 4. shorter symbols, then alphabetical.
pub async fn search_instruments(
    pool: &PgPool,
    search: Option<&str>,
    limit: Option<i64>,
    principal: Option<uuid::Uuid>,
) -> Result<Vec<InstrumentSummary>, sqlx::Error> {
    let limit = clamp_limit(limit);

    sqlx::query_as::<_, InstrumentSummary>(
        "WITH mine AS ( \
             SELECT instrument_id FROM watchlist_item WHERE principal_id = $3 \
             UNION \
             SELECT p.instrument_id FROM position p \
             JOIN principal_portfolio_grant g ON g.portfolio_id = p.portfolio_id \
             WHERE g.principal_id = $3 AND g.can_view AND p.net_qty <> 0 \
             UNION \
             SELECT instrument_id FROM order_state WHERE principal_id = $3 \
         ) \
         SELECT i.id, i.symbol, i.name, i.venue, i.asset_class, i.instrument_class, i.status, \
                EXISTS (SELECT 1 FROM public.instrument_derivative d \
                        WHERE d.underlying_id = i.id OR d.underlying_symbol = i.symbol) AS has_options \
         FROM public.instrument i \
         WHERE i.status = 'ACTIVE' \
           AND ($1::text IS NULL OR i.symbol ILIKE '%' || $1 || '%' OR i.name ILIKE '%' || $1 || '%') \
         ORDER BY \
           CASE \
             WHEN $1::text IS NULL THEN 0 \
             WHEN upper(i.symbol) = upper($4) THEN 0 \
             WHEN i.symbol ILIKE $1 || '%' THEN 1 \
             WHEN i.name ILIKE $1 || '%' OR i.name ILIKE '% ' || $1 || '%' THEN 2 \
             ELSE 3 \
           END, \
           CASE WHEN i.id::text IN (SELECT instrument_id FROM mine) THEN 0 ELSE 1 END, \
           CASE WHEN i.instrument_class = 'OPTION' THEN 1 ELSE 0 END, \
           length(i.symbol), \
           i.symbol \
         LIMIT $2",
    )
    .bind(search.map(escape_like))
    .bind(limit)
    .bind(principal)
    .bind(search)
    .fetch_all(pool)
    .await
}

/// Search instruments
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
    Extension(auth): Extension<AuthContext>,
    Query(params): Query<InstrumentSearch>,
) -> Result<Json<Vec<InstrumentSummary>>, ApiError> {
    search_instruments(state.pool(), params.search.as_deref(), params.limit, Some(auth.principal_id))
        .await
        .map(Json)
        .map_err(|err| ApiError {
            status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("failed to search instruments: {err:?}"),
        })
}

/// One instrument on one side of one strike, for a chain row.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ChainLeg {
    pub instrument_id: i64,
    pub symbol: String,
}

/// One strike of an option chain. `call`/`put` are independently nullable —
/// a venue can list only one side of a strike, and this must say so rather
/// than omit the row or invent the missing leg.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ChainRow {
    pub strike: f64,
    pub call: Option<ChainLeg>,
    pub put: Option<ChainLeg>,
}

#[derive(sqlx::FromRow)]
struct ChainSqlRow {
    strike_price: f64,
    call_id: Option<i64>,
    call_symbol: Option<String>,
    put_id: Option<i64>,
    put_symbol: Option<String>,
}

/// Sorted, distinct expiry dates for an underlying's active option contracts.
pub async fn option_expiries(pool: &PgPool, underlying: &str) -> Result<Vec<NaiveDate>, sqlx::Error> {
    sqlx::query_scalar::<_, NaiveDate>(
        "SELECT DISTINCT d.expiry_date \
         FROM public.instrument_derivative d \
         JOIN public.instrument i ON i.id = d.instrument_id \
         WHERE i.status = 'ACTIVE' AND d.underlying_symbol = $1 AND d.expiry_date IS NOT NULL \
         ORDER BY 1",
    )
    .bind(underlying)
    .fetch_all(pool)
    .await
}

/// One row per distinct strike for `underlying` at `expiry`, call and put
/// self-joined side by side. A strike missing one side comes back with that
/// side `None` — never dropped, never fabricated.
pub async fn option_chain(
    pool: &PgPool,
    underlying: &str,
    expiry: NaiveDate,
) -> Result<Vec<ChainRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ChainSqlRow>(
        "SELECT strikes.strike_price::double precision AS strike_price, \
                call_i.id AS call_id, call_i.symbol AS call_symbol, \
                put_i.id  AS put_id,  put_i.symbol  AS put_symbol \
         FROM (SELECT DISTINCT strike_price FROM public.instrument_derivative \
               WHERE underlying_symbol = $1 AND expiry_date = $2) strikes \
         LEFT JOIN public.instrument_derivative call_d \
                ON call_d.underlying_symbol = $1 AND call_d.expiry_date = $2 \
               AND call_d.strike_price = strikes.strike_price AND call_d.option_kind = 'CALL' \
         LEFT JOIN public.instrument call_i ON call_i.id = call_d.instrument_id AND call_i.status = 'ACTIVE' \
         LEFT JOIN public.instrument_derivative put_d \
                ON put_d.underlying_symbol = $1 AND put_d.expiry_date = $2 \
               AND put_d.strike_price = strikes.strike_price AND put_d.option_kind = 'PUT' \
         LEFT JOIN public.instrument put_i ON put_i.id = put_d.instrument_id AND put_i.status = 'ACTIVE' \
         ORDER BY strikes.strike_price",
    )
    .bind(underlying)
    .bind(expiry)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ChainRow {
            strike: r.strike_price,
            call: match (r.call_id, r.call_symbol) {
                (Some(instrument_id), Some(symbol)) => Some(ChainLeg { instrument_id, symbol }),
                _ => None,
            },
            put: match (r.put_id, r.put_symbol) {
                (Some(instrument_id), Some(symbol)) => Some(ChainLeg { instrument_id, symbol }),
                _ => None,
            },
        })
        .collect())
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct UnderlyingQuery {
    pub underlying: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ChainQuery {
    pub underlying: String,
    pub expiry: NaiveDate,
}

/// List option expiries for an underlying
#[utoipa::path(
    get, path = "/instruments/options/expiries", tag = "orders",
    params(UnderlyingQuery),
    responses((status = 200, description = "Sorted distinct expiry dates", body = [String])),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn list_option_expiries(
    State(state): State<AppState>,
    Query(q): Query<UnderlyingQuery>,
) -> Result<Json<Vec<NaiveDate>>, ApiError> {
    option_expiries(state.pool(), &q.underlying).await.map(Json).map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load expiries: {err:?}"),
    })
}

/// Get an option chain for an underlying and expiry
#[utoipa::path(
    get, path = "/instruments/options/chain", tag = "orders",
    params(ChainQuery),
    responses((status = 200, description = "Strike-sorted chain rows", body = [ChainRow])),
    security(("basic_auth" = []), ("bearer_token" = []))
)]
pub async fn get_option_chain(
    State(state): State<AppState>,
    Query(q): Query<ChainQuery>,
) -> Result<Json<Vec<ChainRow>>, ApiError> {
    option_chain(state.pool(), &q.underlying, q.expiry).await.map(Json).map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("failed to load chain: {err:?}"),
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

        let rows = search_instruments(&pool, Some("ZZTEST"), None, None).await.expect("search");
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();

        assert!(ids.contains(&active), "an ACTIVE instrument must be findable");
        assert!(!ids.contains(&inactive), "an INACTIVE instrument must not be");
    }

    /// No database: exercises `clamp_limit` directly so the clamp itself is
    /// pinned, independent of what any particular search happens to match.
    /// A vacuous search-based assertion (previously used here) would pass
    /// even with `.clamp()` deleted entirely — see fix-round-1 report.
    #[test]
    fn like_wildcards_in_a_query_are_literal() {
        assert_eq!(escape_like("BRK_B"), "BRK\\_B");
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like(r"A\B"), r"A\\B");
        assert_eq!(escape_like("AAPL"), "AAPL");
    }

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

        let rows = search_instruments(&pool, Some(&prefix), Some(2), None).await.expect("search");
        assert_eq!(rows.len(), 2, "LIMIT 2 must return exactly 2 of the 3 matching rows");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn a_search_matches_symbol_or_name() {
        let pool = test_pool().await;
        let id = seed_instrument(&pool, "ZZNAMEHUNT", "ACTIVE").await;

        // seed_instrument sets name = "Test <symbol>", so both paths are covered.
        let by_symbol = search_instruments(&pool, Some("ZZNAMEHUNT"), None, None).await.expect("symbol");
        let by_name = search_instruments(&pool, Some("Test ZZNAMEHUNT"), None, None).await.expect("name");

        assert!(by_symbol.iter().any(|r| r.id == id));
        assert!(by_name.iter().any(|r| r.id == id));
    }

    /// "AAPL" used to come back as AAPB, AAPD, AAPE … AAPL — alphabetical, so
    /// the exact ticker sank below every longer symbol containing it. Ranking
    /// is by match quality: exact symbol, symbol prefix, a word of the name
    /// starting with the query, then plain substring.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn results_rank_by_match_quality_not_alphabet() {
        let pool = test_pool().await;
        let q = unique_query("ZZRANK");
        // Seeded so that alphabetical order would be the exact reverse.
        let substring = seed_named(&pool, &format!("A{q}"), "Unrelated", "SPOT").await;
        let name_word = seed_named(&pool, &unique_query("ZZRNAME"), &format!("Fund {q} Trust"), "SPOT").await;
        let prefix = seed_named(&pool, &format!("{q}A"), "Unrelated", "SPOT").await;
        let exact = seed_named(&pool, &q, "Unrelated", "SPOT").await;
        let seeded = [substring, name_word, prefix, exact];

        let ids = ranked_ids(&search_instruments(&pool, Some(&q.to_lowercase()), None, None).await.expect("search"), &seeded);
        cleanup(&pool, &seeded, None).await;

        assert_eq!(ids, vec![exact, prefix, name_word, substring]);
    }

    /// An option contract on a popular underlying (SPY has ~6,800) must not
    /// crowd stocks/ETFs out of the first page, but stays findable by its
    /// own symbol — ranked last within its match tier rather than hidden.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn options_rank_after_non_options_in_the_same_tier() {
        let pool = test_pool().await;
        let q = unique_query("ZZOPTR");
        let option = seed_named(&pool, &format!("{q}A"), "Unrelated", "OPTION").await;
        let spot = seed_named(&pool, &format!("{q}B"), "Unrelated", "SPOT").await;
        let seeded = [option, spot];

        let ids = ranked_ids(&search_instruments(&pool, Some(&q), None, None).await.expect("search"), &seeded);
        cleanup(&pool, &seeded, None).await;

        assert_eq!(ids, vec![spot, option]);
    }

    /// Within one match tier, what the searching trader already watches,
    /// holds or has ordered comes first. Never outranks a better match.
    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn the_traders_own_instruments_rank_first_within_a_tier() {
        let pool = test_pool().await;
        let q = unique_query("ZZMINE");
        let exact = seed_named(&pool, &q, "Unrelated", "SPOT").await;
        let other = seed_named(&pool, &format!("{q}A"), "Unrelated", "SPOT").await;
        let watched = seed_named(&pool, &format!("{q}B"), "Unrelated", "SPOT").await;
        let seeded = [exact, other, watched];

        let principal = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'HUMAN', 'ACTIVE')")
            .bind(principal)
            .bind(format!("zzmine-{principal}"))
            .execute(&pool)
            .await
            .expect("seed principal");
        sqlx::query("INSERT INTO watchlist_item (principal_id, instrument_id) VALUES ($1, $2)")
            .bind(principal)
            .bind(watched.to_string())
            .execute(&pool)
            .await
            .expect("seed watchlist");

        let mine = ranked_ids(&search_instruments(&pool, Some(&q), None, Some(principal)).await.expect("search"), &seeded);
        let anyone = ranked_ids(&search_instruments(&pool, Some(&q), None, None).await.expect("search"), &seeded);
        cleanup(&pool, &seeded, Some(principal)).await;

        assert_eq!(mine, vec![exact, watched, other], "watched beats unwatched, never the exact match");
        assert_eq!(anyone, vec![exact, other, watched], "no principal, no boost");
    }

    /// A symbol no seeded or catalog row could share, so ranking assertions
    /// only ever see this test's own rows.
    fn unique_query(prefix: &str) -> String {
        format!("{prefix}{}", &uuid::Uuid::new_v4().simple().to_string()[..8]).to_uppercase()
    }

    /// The seeded ids in the order the search returned them.
    fn ranked_ids(rows: &[InstrumentSummary], seeded: &[i64]) -> Vec<i64> {
        rows.iter().map(|r| r.id).filter(|id| seeded.contains(id)).collect()
    }

    async fn seed_named(pool: &sqlx::PgPool, symbol: &str, name: &str, class: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO public.instrument \
                 (symbol, name, venue, asset_class, instrument_class, currency, status, \
                  price_precision, price_increment) \
             VALUES ($1, $2, (SELECT code FROM public.venue ORDER BY code LIMIT 1), 'EQUITY', $3, \
                     (SELECT code FROM public.currency ORDER BY code LIMIT 1), 'ACTIVE', 2, 0.01) \
             RETURNING id",
        )
        .bind(symbol)
        .bind(name)
        .bind(class)
        .fetch_one(pool)
        .await
        .expect("seed instrument")
    }

    /// Ranking tests remove what they seed — they run against the dev
    /// database, and leftover rows would show up in a trader's real searches.
    async fn cleanup(pool: &sqlx::PgPool, ids: &[i64], principal: Option<uuid::Uuid>) {
        if let Some(principal) = principal {
            sqlx::query("DELETE FROM watchlist_item WHERE principal_id = $1")
                .bind(principal)
                .execute(pool)
                .await
                .expect("cleanup watchlist");
            sqlx::query("DELETE FROM principal WHERE id = $1")
                .bind(principal)
                .execute(pool)
                .await
                .expect("cleanup principal");
        }
        sqlx::query("DELETE FROM public.instrument WHERE id = ANY($1)")
            .bind(ids)
            .execute(pool)
            .await
            .expect("cleanup instruments");
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

        let with_rows = search_instruments(&pool, Some(&with_symbol), None, None).await.expect("search");
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
            search_instruments(&pool, Some(&without_symbol), None, None).await.expect("search");
        let without_row = without_rows.iter().find(|r| r.id == without_options).expect("found");
        assert!(!without_row.has_options, "an instrument with no derivative row must not");
    }

    /// Seeds one option leg with an explicit strike/expiry/kind, returning its
    /// instrument id and symbol.
    async fn seed_chain_leg(
        pool: &sqlx::PgPool,
        underlying_symbol: &str,
        kind: &str,
        strike: f64,
        expiry: chrono::NaiveDate,
    ) -> (i64, String) {
        let leg_id = seed_instrument(pool, &format!("{underlying_symbol}{kind}{}", strike as i64), "ACTIVE").await;
        let symbol: String = sqlx::query_scalar("SELECT symbol FROM public.instrument WHERE id = $1")
            .bind(leg_id)
            .fetch_one(pool)
            .await
            .expect("symbol");
        sqlx::query(
            "INSERT INTO public.instrument_derivative \
                (instrument_id, underlying_symbol, option_kind, strike_price, expiry_date) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(leg_id)
        .bind(underlying_symbol)
        .bind(kind)
        .bind(strike)
        .bind(expiry)
        .execute(pool)
        .await
        .expect("seed instrument_derivative");
        (leg_id, symbol)
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn option_expiries_returns_sorted_distinct_dates_for_the_underlying() {
        let pool = test_pool().await;
        let underlying = format!("ZZEXP{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let near = chrono::Utc::now().date_naive() + chrono::Duration::days(7);
        let far = chrono::Utc::now().date_naive() + chrono::Duration::days(30);
        seed_chain_leg(&pool, &underlying, "CALL", 100.0, far).await;
        seed_chain_leg(&pool, &underlying, "PUT", 100.0, far).await; // same date, must not duplicate
        seed_chain_leg(&pool, &underlying, "CALL", 105.0, near).await;

        let dates = option_expiries(&pool, &underlying).await.expect("expiries");
        assert_eq!(dates, vec![near, far], "sorted, distinct, nearest first");
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn option_expiries_is_empty_for_an_underlying_with_no_contracts() {
        let pool = test_pool().await;
        let dates = option_expiries(&pool, "ZZNOCHAIN_NONEXISTENT").await.expect("expiries");
        assert!(dates.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs a live Postgres; run with --ignored"]
    async fn option_chain_returns_null_for_a_side_that_was_never_listed() {
        let pool = test_pool().await;
        let underlying = format!("ZZCHAIN{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let expiry = chrono::Utc::now().date_naive() + chrono::Duration::days(14);
        // 100: both sides. 105: call-only.
        let (call100_id, call100_sym) = seed_chain_leg(&pool, &underlying, "CALL", 100.0, expiry).await;
        let (put100_id, put100_sym) = seed_chain_leg(&pool, &underlying, "PUT", 100.0, expiry).await;
        let (call105_id, call105_sym) = seed_chain_leg(&pool, &underlying, "CALL", 105.0, expiry).await;

        let rows = option_chain(&pool, &underlying, expiry).await.expect("chain");
        assert_eq!(rows.len(), 2, "one row per distinct strike");

        let row100 = rows.iter().find(|r| r.strike == 100.0).expect("strike 100");
        assert_eq!(row100.call.as_ref().unwrap().instrument_id, call100_id);
        assert_eq!(row100.call.as_ref().unwrap().symbol, call100_sym);
        assert_eq!(row100.put.as_ref().unwrap().instrument_id, put100_id);
        assert_eq!(row100.put.as_ref().unwrap().symbol, put100_sym);

        let row105 = rows.iter().find(|r| r.strike == 105.0).expect("strike 105");
        assert_eq!(row105.call.as_ref().unwrap().instrument_id, call105_id);
        assert_eq!(row105.call.as_ref().unwrap().symbol, call105_sym);
        assert!(row105.put.is_none(), "no put was ever listed at 105 — must be null, not omitted or erroring");
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
