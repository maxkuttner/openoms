//! Pre-trade risk engine.
//!
//! Layering (functional core, imperative shell):
//! - `domain::orders::risk`  — pure decision functions, no I/O
//! - `RiskDataProvider`      — port for sourcing risk config + exposure
//! - `PgRiskDataProvider`    — Postgres adapter; runs inside the caller's
//!                             transaction and locks the `risk_limits` row
//!                             (`FOR UPDATE`) so concurrent submits for the
//!                             same scope are serialized until commit.
//!
//! A scope with no `risk_limits` row has no limits configured and passes
//! unchecked (and is therefore also not serialized — harmless, since there
//! is nothing to enforce).

use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::domain::orders::commands::SubmitOrder;
use crate::domain::orders::risk::{
    evaluate, Exposure, RiskLimits, RiskRejection, TradingState,
};

/// The (portfolio, instrument) slice that limits and exposure are keyed by.
pub struct RiskScope<'a> {
    pub portfolio_id: Uuid,
    pub instrument_id: &'a str,
}

/// Risk configuration for one scope, as stored in `risk_limits`:
/// the trading-state gate plus the numeric limits.
pub struct RiskConfig {
    pub trading_state: TradingState,
    pub limits: RiskLimits,
}

#[derive(Debug)]
pub enum RiskCheckError {
    /// The order violates a configured limit or trading-state gate.
    Rejected(RiskRejection),
    /// Risk config/exposure could not be loaded; treat as an internal error.
    Data(sqlx::Error),
}

impl From<sqlx::Error> for RiskCheckError {
    fn from(err: sqlx::Error) -> Self {
        Self::Data(err)
    }
}

/// Port for sourcing the data the pure risk checks need.
/// Swap `PgRiskDataProvider` for a cached/projected implementation later
/// without touching the engine or `domain::orders::risk`.
#[allow(async_fn_in_trait)] // engine is generic over P, so Send leaks through the concrete type
pub trait RiskDataProvider {
    /// Risk config for the scope, or `None` if no limits are configured.
    async fn risk_config(&mut self, scope: &RiskScope<'_>) -> Result<Option<RiskConfig>, sqlx::Error>;

    /// Current signed exposure for the scope (buys positive, sells negative).
    async fn exposure(&mut self, scope: &RiskScope<'_>) -> Result<Exposure, sqlx::Error>;
}

pub struct RiskEngine<P> {
    provider: P,
}

impl<P: RiskDataProvider> RiskEngine<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Run all pre-trade checks for a submit command.
    /// `Ok(())` means the order may proceed; no configured limits means pass.
    /// `contract_size` is the instrument's notional multiplier (100 for options,
    /// 1 for equities/crypto). Quantity limits stay in native units (contracts),
    /// while notional limits see `qty * price * contract_size`.
    pub async fn check_submit(
        &mut self,
        portfolio_id: Uuid,
        cmd: &SubmitOrder,
        contract_size: f64,
    ) -> Result<(), RiskCheckError> {
        let scope = RiskScope {
            portfolio_id,
            instrument_id: &cmd.instrument_id,
        };

        // No risk_limits row => nothing to enforce for this scope.
        let Some(config) = self.provider.risk_config(&scope).await? else {
            return Ok(());
        };

        let exposure = self.provider.exposure(&scope).await?;

        // One pure evaluation covers the state gate and every numeric limit.
        // The multiplier stays separate from the price so it is applied once.
        // Market orders carry no limit price and intentionally skip notional
        // checks when no order valuation is supplied (documented in risk.rs).
        let input = risk::RiskInput {
            order: risk::OrderInput {
                side: match cmd.side {
                    crate::domain::orders::state::OrderSide::Buy => risk::Side::Buy,
                    crate::domain::orders::state::OrderSide::Sell => risk::Side::Sell,
                },
                quantity: cmd.quantity,
                limit_price: cmd.limit_price,
                multiplier: contract_size,
            },
            exposure,
            limits: config.limits,
            state: config.trading_state,
            valuation: risk::Valuation {
                order_price: cmd.limit_price,
                position_price: cmd.limit_price,
                reference_price: None,
            },
        };
        evaluate(&input).map_err(RiskCheckError::Rejected)
    }
}

/// Postgres-backed provider. Borrows the caller's connection so the checks
/// run inside the same transaction as the subsequent `order_state` insert.
pub struct PgRiskDataProvider<'c> {
    conn: &'c mut PgConnection,
}

impl<'c> PgRiskDataProvider<'c> {
    pub fn new(conn: &'c mut PgConnection) -> Self {
        Self { conn }
    }
}

impl RiskDataProvider for PgRiskDataProvider<'_> {
    async fn risk_config(&mut self, scope: &RiskScope<'_>) -> Result<Option<RiskConfig>, sqlx::Error> {
        // FOR UPDATE closes the check-then-insert race: concurrent submits on
        // the same scope block here until the holding transaction commits, so
        // the exposure read below always sees the winner's inserted order.
        let row = sqlx::query(
             "SELECT trading_state, \
                    max_order_quantity::double precision    AS max_order_quantity, \
                    max_order_notional::double precision    AS max_order_notional, \
                    max_position_quantity::double precision AS max_position_quantity, \
                    max_position_notional::double precision AS max_position_notional \
             FROM risk_limits \
             WHERE portfolio_id = $1 AND instrument_id = $2 \
             FOR UPDATE",
        )
        .bind(scope.portfolio_id)
        .bind(scope.instrument_id)
        .fetch_optional(&mut *self.conn)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(RiskConfig {
            trading_state: parse_trading_state(row.get::<String, _>("trading_state"))?,
            limits: RiskLimits {
                max_order_qty: row.get("max_order_quantity"),
                max_order_notional: row.get("max_order_notional"),
                max_position_qty: row.get("max_position_quantity"),
                max_position_notional: row.get("max_position_notional"),
                ..RiskLimits::default()
            },
        }))
    }

    async fn exposure(&mut self, scope: &RiskScope<'_>) -> Result<Exposure, sqlx::Error> {
        // One snapshot: the signed position and each side's working leaves are
        // read in a single statement, so a fill moving quantity from a working
        // order into the position cannot make exposure disappear between reads.
        let row = sqlx::query(
            "SELECT \
                COALESCE((SELECT net_qty::double precision FROM position \
                          WHERE portfolio_id = $1 AND instrument_id = $2), 0)::double precision AS position_qty, \
                COALESCE(SUM(CASE WHEN side = 'buy' THEN leaves_qty ELSE 0 END), 0)::double precision AS working_buy_qty, \
                COALESCE(SUM(CASE WHEN side = 'sell' THEN leaves_qty ELSE 0 END), 0)::double precision AS working_sell_qty \
             FROM order_state \
             WHERE portfolio_id = $1 AND instrument_id = $2 \
               AND status IN ('submitted', 'routed', 'partially_filled')",
        )
        .bind(scope.portfolio_id)
        .bind(scope.instrument_id)
        .fetch_one(&mut *self.conn)
        .await?;

        Ok(Exposure {
            position_qty: row.get("position_qty"),
            working_buy_qty: row.get("working_buy_qty"),
            working_sell_qty: row.get("working_sell_qty"),
        })
    }
}

fn parse_trading_state(value: String) -> Result<TradingState, sqlx::Error> {
    match value.as_str() {
        "ACTIVE" => Ok(TradingState::Active),
        "REDUCING" => Ok(TradingState::Reducing),
        "HALTED" => Ok(TradingState::Halted),
        other => Err(sqlx::Error::ColumnDecode {
            index: "trading_state".into(),
            source: format!("unknown trading_state: {other}").into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::orders::state::{OrderSide, OrderType, TimeInForce};

    /// In-memory provider: proves the engine is testable without a database.
    struct StubProvider {
        config: Option<RiskConfig>,
        exposure: Exposure,
    }

    impl RiskDataProvider for StubProvider {
        async fn risk_config(&mut self, _: &RiskScope<'_>) -> Result<Option<RiskConfig>, sqlx::Error> {
            Ok(self.config.take())
        }

        async fn exposure(&mut self, _: &RiskScope<'_>) -> Result<Exposure, sqlx::Error> {
            Ok(self.exposure)
        }
    }

    fn submit(side: OrderSide, quantity: f64, limit_price: Option<f64>) -> SubmitOrder {
        SubmitOrder {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: "c-1".to_string(),
            portfolio_id: Uuid::new_v4().to_string(),
            account_id: Uuid::new_v4().to_string(),
            instrument_id: Uuid::new_v4().to_string(),
            side,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Day,
            limit_price,
            quantity,
        }
    }

    async fn run(provider: StubProvider, cmd: &SubmitOrder) -> Result<(), RiskCheckError> {
        run_with(provider, cmd, 1.0).await
    }

    async fn run_with(provider: StubProvider, cmd: &SubmitOrder, contract_size: f64) -> Result<(), RiskCheckError> {
        RiskEngine::new(provider)
            .check_submit(Uuid::new_v4(), cmd, contract_size)
            .await
    }

    fn config(trading_state: TradingState, limits: RiskLimits) -> Option<RiskConfig> {
        Some(RiskConfig { trading_state, limits })
    }

    const NO_LIMITS: RiskLimits = RiskLimits {
        max_order_qty: None,
        max_order_notional: None,
        max_position_qty: None,
        max_position_notional: None,
        price_band_pct: None,
        max_orders_per_sec: None,
    };

    #[tokio::test]
    async fn no_config_passes() {
        let provider = StubProvider {
            config: None,
            exposure: Exposure { position_qty: 0.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
        };
        let r = run(provider, &submit(OrderSide::Buy, 1_000_000.0, None)).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn halted_scope_rejects() {
        let provider = StubProvider {
            config: config(TradingState::Halted, NO_LIMITS),
            exposure: Exposure { position_qty: 0.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
        };
        let err = run(provider, &submit(OrderSide::Buy, 1.0, None)).await.unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "trading_halted"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn order_qty_breach_rejects() {
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits { max_order_qty: Some(100.0), ..NO_LIMITS },
            ),
            exposure: Exposure { position_qty: 0.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
        };
        let err = run(provider, &submit(OrderSide::Buy, 150.0, None)).await.unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "max_order_qty"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exposure_feeds_position_limit() {
        // position 80 + working 15 + buy 10 = 105 > 100
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits { max_position_qty: Some(100.0), ..NO_LIMITS },
            ),
            exposure: Exposure { position_qty: 80.0, working_buy_qty: 15.0, working_sell_qty: 0.0 },
        };
        let err = run(provider, &submit(OrderSide::Buy, 10.0, None)).await.unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "max_position_quantity"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn contract_size_scales_notional() {
        // 1 option contract @ $3 limit. Bare notional $3 passes a $100 cap, but
        // with contract_size 100 the notional is $300 and must breach.
        let limits = RiskLimits { max_order_notional: Some(100.0), ..NO_LIMITS };

        let pass = run_with(
            StubProvider {
                config: config(TradingState::Active, limits),
                exposure: Exposure { position_qty: 0.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
            },
            &submit(OrderSide::Buy, 1.0, Some(3.0)),
            1.0,
        )
        .await;
        assert!(pass.is_ok(), "notional $3 should pass a $100 cap at multiplier 1");

        let err = run_with(
            StubProvider {
                config: config(TradingState::Active, limits),
                exposure: Exposure { position_qty: 0.0, working_buy_qty: 0.0, working_sell_qty: 0.0 },
            },
            &submit(OrderSide::Buy, 1.0, Some(3.0)),
            100.0,
        )
        .await
        .unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "max_order_notional"),
            other => panic!("expected notional rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn opposing_working_orders_reserve_separately() {
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits { max_position_qty: Some(100.0), ..NO_LIMITS },
            ),
            exposure: Exposure {
                position_qty: 0.0,
                working_buy_qty: 100.0,
                working_sell_qty: 100.0,
            },
        };
        let err = run(provider, &submit(OrderSide::Buy, 100.0, None)).await.unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "max_position_quantity"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn market_order_without_price_skips_notional() {
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits { max_order_notional: Some(1.0), ..NO_LIMITS },
            ),
            exposure: Exposure::default(),
        };
        let r = run(provider, &submit(OrderSide::Buy, 50.0, None)).await;
        assert!(r.is_ok());
    }

    /// A disposable database selected explicitly, never the shared dev config.
    /// Run with `COLO_TEST_DATABASE_URL=postgres://… cargo test -- --ignored`.
    async fn isolated_pool() -> sqlx::PgPool {
        let url = std::env::var("COLO_TEST_DATABASE_URL")
            .expect("set COLO_TEST_DATABASE_URL to a disposable database");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect isolated test database")
    }

    struct Fixture {
        principal_id: Uuid,
        portfolio_id: Uuid,
        account_id: Uuid,
        broker_code: String,
        instrument_id: String,
    }

    async fn seed_fixture(pool: &sqlx::PgPool) -> Fixture {
        let suffix = Uuid::new_v4().simple().to_string();
        let fixture = Fixture {
            principal_id: Uuid::new_v4(),
            portfolio_id: Uuid::new_v4(),
            account_id: Uuid::new_v4(),
            broker_code: format!("test-{}", &suffix[..12]),
            instrument_id: format!("TEST{}", &suffix[..12]),
        };
        sqlx::query("INSERT INTO principal (id, code, principal_type, status) VALUES ($1, $2, 'STRATEGY', 'ACTIVE')")
            .bind(fixture.principal_id)
            .bind(format!("p-{suffix}"))
            .execute(pool)
            .await
            .expect("seed principal");
        sqlx::query("INSERT INTO portfolio (id, code, name, status) VALUES ($1, $2, 'risk test', 'ACTIVE')")
            .bind(fixture.portfolio_id)
            .bind(format!("pf-{suffix}"))
            .execute(pool)
            .await
            .expect("seed portfolio");
        sqlx::query("INSERT INTO broker_connection (code, broker_code, environment, status) VALUES ($1, 'TEST', 'PAPER', 'ACTIVE')")
            .bind(&fixture.broker_code)
            .execute(pool)
            .await
            .expect("seed broker connection");
        sqlx::query("INSERT INTO account (id, code, broker_connection_code, external_account_ref, status) VALUES ($1, $2, $3, $4, 'ACTIVE')")
            .bind(fixture.account_id)
            .bind(format!("a-{suffix}"))
            .bind(&fixture.broker_code)
            .bind(format!("ext-{suffix}"))
            .execute(pool)
            .await
            .expect("seed account");
        fixture
    }

    async fn cleanup_fixture(pool: &sqlx::PgPool, fixture: &Fixture) {
        for (sql, bind) in [
            ("DELETE FROM order_state WHERE portfolio_id = $1", fixture.portfolio_id),
            ("DELETE FROM risk_limits WHERE portfolio_id = $1", fixture.portfolio_id),
            ("DELETE FROM position WHERE portfolio_id = $1", fixture.portfolio_id),
            ("DELETE FROM account WHERE id = $1", fixture.account_id),
        ] {
            let _ = sqlx::query(sql).bind(bind).execute(pool).await;
        }
        let _ = sqlx::query("DELETE FROM broker_connection WHERE code = $1")
            .bind(&fixture.broker_code)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM portfolio WHERE id = $1")
            .bind(fixture.portfolio_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM principal WHERE id = $1")
            .bind(fixture.principal_id)
            .execute(pool)
            .await;
    }

    async fn seed_order(
        pool: &sqlx::PgPool,
        fixture: &Fixture,
        side: &str,
        leaves: f64,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO order_state \
             (order_id, client_order_id, principal_id, portfolio_id, account_id, broker_connection_code, \
              instrument_id, side, order_type, time_in_force, original_qty, leaves_qty, cum_qty, status, version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'limit', 'day', $9, $9, 0, $10, 0)",
        )
        .bind(Uuid::new_v4())
        .bind(format!("c-{}", Uuid::new_v4()))
        .bind(fixture.principal_id)
        .bind(fixture.portfolio_id)
        .bind(fixture.account_id)
        .bind(&fixture.broker_code)
        .bind(&fixture.instrument_id)
        .bind(side)
        .bind(leaves)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed order");
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn directional_exposure_reads_one_snapshot() {
        let pool = isolated_pool().await;
        let fixture = seed_fixture(&pool).await;
        sqlx::query("INSERT INTO position (portfolio_id, instrument_id, net_qty, avg_cost, realized_pnl) VALUES ($1, $2, 25, 100, 0)")
            .bind(fixture.portfolio_id)
            .bind(&fixture.instrument_id)
            .execute(&pool)
            .await
            .expect("seed position");
        seed_order(&pool, &fixture, "buy", 100.0, "submitted").await;
        seed_order(&pool, &fixture, "sell", 100.0, "partially_filled").await;

        let mut tx = pool.begin().await.expect("begin");
        let exposure = PgRiskDataProvider::new(&mut tx)
            .exposure(&RiskScope {
                portfolio_id: fixture.portfolio_id,
                instrument_id: &fixture.instrument_id,
            })
            .await
            .expect("read exposure");
        tx.rollback().await.expect("rollback");

        cleanup_fixture(&pool, &fixture).await;

        assert_eq!(exposure.position_qty, 25.0);
        assert_eq!(exposure.working_buy_qty, 100.0);
        assert_eq!(exposure.working_sell_qty, 100.0);
    }

    #[tokio::test]
    #[ignore = "needs COLO_TEST_DATABASE_URL pointing at a disposable Postgres"]
    async fn concurrent_submits_share_capacity() {
        let pool = isolated_pool().await;
        let fixture = seed_fixture(&pool).await;
        sqlx::query("INSERT INTO risk_limits (portfolio_id, instrument_id, trading_state, max_position_quantity) VALUES ($1, $2, 'ACTIVE', 100)")
            .bind(fixture.portfolio_id)
            .bind(&fixture.instrument_id)
            .execute(&pool)
            .await
            .expect("seed risk limits");
        let cmd = SubmitOrder {
            order_id: Uuid::new_v4().to_string(),
            client_order_id: "race".into(),
            portfolio_id: fixture.portfolio_id.to_string(),
            account_id: fixture.account_id.to_string(),
            instrument_id: fixture.instrument_id.clone(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::Day,
            limit_price: Some(100.0),
            quantity: 60.0,
        };

        let first_pool = pool.clone();
        let first_fixture = Fixture {
            principal_id: fixture.principal_id,
            portfolio_id: fixture.portfolio_id,
            account_id: fixture.account_id,
            broker_code: fixture.broker_code.clone(),
            instrument_id: fixture.instrument_id.clone(),
        };
        let first_cmd = cmd.clone();
        let first = tokio::spawn(async move {
            let mut tx = first_pool.begin().await.expect("begin first");
            RiskEngine::new(PgRiskDataProvider::new(&mut tx))
                .check_submit(first_fixture.portfolio_id, &first_cmd, 1.0)
                .await
                .expect("first submit passes");
            sqlx::query(
                "INSERT INTO order_state \
                 (order_id, client_order_id, principal_id, portfolio_id, account_id, broker_connection_code, \
                  instrument_id, side, order_type, time_in_force, limit_price, original_qty, leaves_qty, cum_qty, status, version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'buy', 'limit', 'day', 100, 60, 60, 0, 'submitted', 0)",
            )
            .bind(Uuid::new_v4())
            .bind(format!("c-{}", Uuid::new_v4()))
            .bind(first_fixture.principal_id)
            .bind(first_fixture.portfolio_id)
            .bind(first_fixture.account_id)
            .bind(&first_fixture.broker_code)
            .bind(&first_fixture.instrument_id)
            .execute(&mut *tx)
            .await
            .expect("insert first order");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tx.commit().await.expect("commit first");
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut tx = pool.begin().await.expect("begin second");
        let second = RiskEngine::new(PgRiskDataProvider::new(&mut tx))
            .check_submit(fixture.portfolio_id, &cmd, 1.0)
            .await;
        tx.rollback().await.expect("rollback second");
        first.await.expect("first task");

        cleanup_fixture(&pool, &fixture).await;

        match second {
            Err(RiskCheckError::Rejected(r)) => assert_eq!(r.code, "max_position_quantity"),
            other => panic!("expected the second submit to be rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn within_limits_passes() {
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits {
                    max_order_qty: Some(100.0),
                    max_order_notional: Some(10_000.0),
                    max_position_qty: Some(500.0),
                    max_position_notional: Some(50_000.0),
                    ..NO_LIMITS
                },
            ),
            exposure: Exposure { position_qty: 50.0, working_buy_qty: 10.0, working_sell_qty: 0.0 },
        };
        let r = run(provider, &submit(OrderSide::Buy, 20.0, Some(100.0))).await;
        assert!(r.is_ok());
    }
}
