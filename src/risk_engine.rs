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
    check_limit, check_trading_state, Exposure, RiskLimits, RiskRejection, TradingState,
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
    pub async fn check_submit(
        &mut self,
        portfolio_id: Uuid,
        cmd: &SubmitOrder,
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

        // Cheap gate first, then the limit math.
        check_trading_state(config.trading_state, cmd.side, exposure.position_qty)
            .map_err(RiskCheckError::Rejected)?;

        // est_price comes from the limit price; market orders carry None and
        // intentionally skip the notional checks (documented in risk.rs tests).
        check_limit(cmd.side, cmd.quantity, cmd.limit_price, config.limits, exposure)
            .map_err(RiskCheckError::Rejected)
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
                max_order_quantity: row.get("max_order_quantity"),
                max_order_notional: row.get("max_order_notional"),
                max_position_quantity: row.get("max_position_quantity"),
                max_position_notional: row.get("max_position_notional"),
            },
        }))
    }

    async fn exposure(&mut self, scope: &RiskScope<'_>) -> Result<Exposure, sqlx::Error> {
        // position_qty: O(1) read of the signed net holding from the `position`
        // projection (maintained on fills; covers external fills too). 0 if flat.
        let position_qty: f64 = sqlx::query_scalar(
            "SELECT COALESCE((SELECT net_qty::double precision FROM position \
                              WHERE portfolio_id = $1 AND instrument_id = $2), 0)",
        )
        .bind(scope.portfolio_id)
        .bind(scope.instrument_id)
        .fetch_one(&mut *self.conn)
        .await?;

        // working_qty: signed open quantity of live (unfilled) orders only.
        let working_qty: f64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(CASE WHEN side = 'buy' THEN leaves_qty ELSE -leaves_qty END), 0)::double precision \
             FROM order_state \
             WHERE portfolio_id = $1 AND instrument_id = $2 \
               AND status IN ('submitted', 'routed', 'partially_filled')",
        )
        .bind(scope.portfolio_id)
        .bind(scope.instrument_id)
        .fetch_one(&mut *self.conn)
        .await?;

        Ok(Exposure { position_qty, working_qty })
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
        RiskEngine::new(provider)
            .check_submit(Uuid::new_v4(), cmd)
            .await
    }

    fn config(trading_state: TradingState, limits: RiskLimits) -> Option<RiskConfig> {
        Some(RiskConfig { trading_state, limits })
    }

    const NO_LIMITS: RiskLimits = RiskLimits {
        max_order_quantity: None,
        max_order_notional: None,
        max_position_quantity: None,
        max_position_notional: None,
    };

    #[tokio::test]
    async fn no_config_passes() {
        let provider = StubProvider {
            config: None,
            exposure: Exposure { position_qty: 0.0, working_qty: 0.0 },
        };
        let r = run(provider, &submit(OrderSide::Buy, 1_000_000.0, None)).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn halted_scope_rejects() {
        let provider = StubProvider {
            config: config(TradingState::Halted, NO_LIMITS),
            exposure: Exposure { position_qty: 0.0, working_qty: 0.0 },
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
                RiskLimits { max_order_quantity: Some(100.0), ..NO_LIMITS },
            ),
            exposure: Exposure { position_qty: 0.0, working_qty: 0.0 },
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
                RiskLimits { max_position_quantity: Some(100.0), ..NO_LIMITS },
            ),
            exposure: Exposure { position_qty: 80.0, working_qty: 15.0 },
        };
        let err = run(provider, &submit(OrderSide::Buy, 10.0, None)).await.unwrap_err();
        match err {
            RiskCheckError::Rejected(r) => assert_eq!(r.code, "max_position_quantity"),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn within_limits_passes() {
        let provider = StubProvider {
            config: config(
                TradingState::Active,
                RiskLimits {
                    max_order_quantity: Some(100.0),
                    max_order_notional: Some(10_000.0),
                    max_position_quantity: Some(500.0),
                    max_position_notional: Some(50_000.0),
                },
            ),
            exposure: Exposure { position_qty: 50.0, working_qty: 10.0 },
        };
        let r = run(provider, &submit(OrderSide::Buy, 20.0, Some(100.0))).await;
        assert!(r.is_ok());
    }
}
