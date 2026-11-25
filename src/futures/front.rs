//! Front-month resolution: which listed contract of a root is the one to trade.
//!
//! Only dated futures count (`instrument_class = 'FUTURE' AND expiry_date IS NOT
//! NULL`), so perpetuals never enter. Roll dates are venue-local (America/Chicago).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

/// One listed contract of a root.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct FutureContract {
    pub instrument_id: i64,
    pub symbol: String,
    pub expiry_date: NaiveDate,
    pub roll_date: NaiveDate,
    pub contract_size: f64,
}

/// The day the front month moves on: `roll_offset_days` calendar days before expiry.
pub fn roll_date(expiry: NaiveDate, roll_offset_days: i32) -> NaiveDate {
    expiry - chrono::Duration::days(i64::from(roll_offset_days))
}

/// The calendar date at the venue (America/Chicago) at `now`.
pub fn venue_today(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&chrono_tz::America::Chicago).date_naive()
}

/// Front and next contract of `root` as of `today`. Front is the earliest ACTIVE
/// contract whose roll date is after `today`; next is the one after it.
pub async fn front_and_next(
    pool: &PgPool,
    root: &str,
    today: NaiveDate,
) -> Result<(Option<FutureContract>, Option<FutureContract>), sqlx::Error> {
    let rows: Vec<(i64, String, NaiveDate, f64, i32)> = sqlx::query_as(
        "SELECT i.id, i.symbol, d.expiry_date, i.contract_size::float8, r.roll_offset_days \
         FROM instrument i \
         JOIN instrument_derivative d ON d.instrument_id = i.id \
         JOIN future_root r ON r.code = d.underlying_symbol \
         WHERE i.instrument_class = 'FUTURE' AND d.expiry_date IS NOT NULL \
           AND i.status = 'ACTIVE' AND d.underlying_symbol = $1 \
         ORDER BY d.expiry_date, i.id",
    )
    .bind(root)
    .fetch_all(pool)
    .await?;
    let mut live = rows.into_iter().filter_map(|(instrument_id, symbol, expiry_date, contract_size, offset)| {
        let roll = roll_date(expiry_date, offset);
        (roll > today).then_some(FutureContract { instrument_id, symbol, expiry_date, roll_date: roll, contract_size })
    });
    Ok((live.next(), live.next()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn es_dec_2026_rolls_on_dec_10() {
        assert_eq!(roll_date(d(2026, 12, 18), 8), d(2026, 12, 10));
    }

    #[test]
    fn venue_today_is_chicago() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(venue_today(t("2026-12-10T03:00:00Z")), d(2026, 12, 9));
        assert_eq!(venue_today(t("2026-12-10T07:00:00Z")), d(2026, 12, 10));
    }

    /// Serialises the DB tests: they share rows.
    static DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn isolated_pool() -> PgPool {
        let url = std::env::var("OMS_TEST_DATABASE_URL").expect("set OMS_TEST_DATABASE_URL to a disposable database");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO oms, public").execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect isolated test database")
    }

    /// Replaces the ES futures with the given (symbol, expiry, status) rows.
    async fn seed(pool: &PgPool, rows: &[(&str, NaiveDate, &str)]) {
        sqlx::query(
            "DELETE FROM instrument WHERE id IN (SELECT instrument_id FROM instrument_derivative WHERE underlying_symbol = 'ES')",
        )
        .execute(pool)
        .await
        .unwrap();
        for (symbol, expiry, status) in rows {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO instrument (symbol, venue, name, asset_class, instrument_class, currency, status, contract_size, price_precision, price_increment) \
                 VALUES ($1, 'XCME', $1, 'INDEX', 'FUTURE', 'USD', $2, 50, 2, 0.25) RETURNING id",
            )
            .bind(symbol)
            .bind(status)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO instrument_derivative (instrument_id, underlying_symbol, expiry_date) VALUES ($1, 'ES', $2)",
            )
            .bind(id)
            .bind(expiry)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn front_switches_on_roll_date_in_chicago() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        seed(&pool, &[("ESZ6", d(2026, 12, 18), "ACTIVE"), ("ESH7", d(2027, 3, 19), "ACTIVE")]).await;
        let (front, next) = front_and_next(&pool, "ES", d(2026, 12, 9)).await.unwrap();
        assert_eq!(front.as_ref().map(|c| c.symbol.as_str()), Some("ESZ6"));
        assert_eq!(next.as_ref().map(|c| c.symbol.as_str()), Some("ESH7"));
        let f = front.unwrap();
        assert_eq!((f.roll_date, f.expiry_date, f.contract_size), (d(2026, 12, 10), d(2026, 12, 18), 50.0));
        let (front, next) = front_and_next(&pool, "ES", d(2026, 12, 10)).await.unwrap();
        assert_eq!(front.as_ref().map(|c| c.symbol.as_str()), Some("ESH7"));
        assert!(next.is_none());
    }

    #[tokio::test]
    #[ignore = "needs OMS_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn expired_contract_is_never_front() {
        let _g = DB.lock().await;
        let pool = isolated_pool().await;
        seed(&pool, &[("ESZ6", d(2026, 12, 18), "EXPIRED"), ("ESH7", d(2027, 3, 19), "ACTIVE")]).await;
        let (front, next) = front_and_next(&pool, "ES", d(2026, 11, 1)).await.unwrap();
        assert_eq!(front.map(|c| c.symbol), Some("ESH7".to_string()));
        assert!(next.is_none());
    }
}
