use crate::models::{Account, Connector, Order};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Clone)]
pub struct AccountRepo {
    pool: PgPool,
}

impl AccountRepo {
    // Create a new AccountRepo
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // Check if an account exists
    pub async fn exists(&self, account_id: i64) -> Result<bool, sqlx::Error> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM account WHERE account_id = $1)")
                .bind(account_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }

    /// Find a specific account by its primary key
    pub async fn get_by_id(&self, account_id: i64) -> Result<Option<Account>, sqlx::Error> {
        sqlx::query_as::<_, Account>("SELECT * FROM account WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Find all accounts belonging to a specific user
    pub async fn get_all_by_user(&self, user_id: i64) -> Result<Vec<Account>, sqlx::Error> {
        sqlx::query_as::<_, Account>(
            "SELECT * FROM account WHERE user_id = $1 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
    }
}

#[derive(Clone)]
pub struct ConnectorRepo {
    pool: PgPool,
}

impl ConnectorRepo {
    // Create a new ConnectorRepo
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn get_by_id(
        &self,
        connector_id: i64,
        account_id: i64,
    ) -> Result<Option<Connector>, sqlx::Error> {
        sqlx::query_as::<_, Connector>(
            r#"
            SELECT 
                c.connector_id, 
                c.account_id, 
                c.provider_id, 
                c.environment, 
                c.base_currency, 
                c.external_account_id, 
                c.api_key, 
                c.connection_status, 
                c.last_test_at,
                p.name as provider_name, -- Ensure this matches your struct field name
                p.code as provider_code -- Ensure this matches your struct field name
            FROM connector c
            JOIN provider p ON c.provider_id = p.provider_id
            WHERE c.connector_id = $1 
            AND c.account_id = $2
            "#,
        )
        .bind(connector_id)
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
    }
}

#[derive(Clone)]
pub struct OrderRepo {
    pool: PgPool,
}

impl OrderRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn get_by_client_order_id(
        &self,
        account_id: i64,
        client_order_id: &str,
    ) -> Result<Option<Order>, sqlx::Error> {
        sqlx::query_as::<_, Order>(
            r#"
            SELECT
                order_id,
                account_id,
                connector_id,
                instrument_id,
                client_order_id,
                side,
                order_type,
                quantity::double precision AS quantity,
                limit_price::double precision AS limit_price,
                filled_quantity::double precision AS filled_quantity,
                status,
                external_order_id,
                error_message,
                created_at,
                updated_at
            FROM orders
            WHERE account_id = $1 AND client_order_id = $2
            "#,
        )
        .bind(account_id)
        .bind(client_order_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn resolve_instrument_id(
        &self,
        connector_id: i64,
        symbol: &str,
    ) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT pi.instrument_id
            FROM connector c
            JOIN provider_instrument pi ON pi.provider_id = c.provider_id
            WHERE c.connector_id = $1
              AND UPPER(pi.provider_symbol) = UPPER($2)
            LIMIT 1
            "#,
        )
        .bind(connector_id)
        .bind(symbol)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn insert_received_order(
        &self,
        account_id: i64,
        connector_id: i64,
        instrument_id: i64,
        client_order_id: &str,
        side: &str,
        order_type: &str,
        quantity: f64,
        limit_price: Option<f64>,
    ) -> Result<Order, sqlx::Error> {
        sqlx::query_as::<_, Order>(
            r#"
            INSERT INTO orders (
                account_id,
                connector_id,
                instrument_id,
                client_order_id,
                side,
                order_type,
                quantity,
                limit_price,
                filled_quantity,
                status
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, 'received')
            RETURNING
                order_id,
                account_id,
                connector_id,
                instrument_id,
                client_order_id,
                side,
                order_type,
                quantity::double precision AS quantity,
                limit_price::double precision AS limit_price,
                filled_quantity::double precision AS filled_quantity,
                status,
                external_order_id,
                error_message,
                created_at,
                updated_at
            "#,
        )
        .bind(account_id)
        .bind(connector_id)
        .bind(instrument_id)
        .bind(client_order_id)
        .bind(side)
        .bind(order_type)
        .bind(quantity)
        .bind(limit_price)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn update_order_status(
        &self,
        order_id: Uuid,
        status: &str,
        external_order_id: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<Order, sqlx::Error> {
        sqlx::query_as::<_, Order>(
            r#"
            UPDATE orders
            SET
                status = $2,
                external_order_id = COALESCE($3, external_order_id),
                error_message = $4,
                updated_at = CURRENT_TIMESTAMP
            WHERE order_id = $1
            RETURNING
                order_id,
                account_id,
                connector_id,
                instrument_id,
                client_order_id,
                side,
                order_type,
                quantity::double precision AS quantity,
                limit_price::double precision AS limit_price,
                filled_quantity::double precision AS filled_quantity,
                status,
                external_order_id,
                error_message,
                created_at,
                updated_at
            "#,
        )
        .bind(order_id)
        .bind(status)
        .bind(external_order_id)
        .bind(error_message)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn append_event(
        &self,
        order_id: Uuid,
        event_type: &str,
        status_after: &str,
        payload_json: &serde_json::Value,
        source: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO order_events (
                order_id,
                event_type,
                status_after,
                payload_json,
                source
            )
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(order_id)
        .bind(event_type)
        .bind(status_after)
        .bind(payload_json)
        .bind(source)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct PositionProjectorRepo {
    pool: PgPool,
}

impl PositionProjectorRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    

    // apply projection based on an order acknolwedged msg
    pub async fn apply_ack_fill_projection(
        &self,
        projector_event_id: &str,
        order_id: Uuid,
        account_id: i64,
        connector_id: i64,
        side: &str,
        quantity: f64,
        fill_price: f64,
    ) -> Result<(), sqlx::Error> {
        
        let mut tx = self.pool.begin().await?;

        let inserted = sqlx::query_scalar::<_, bool>(
            r#"
            INSERT INTO position_projection_log (projector_event_id, order_id)
            VALUES ($1, $2)
            ON CONFLICT (projector_event_id) DO NOTHING
            RETURNING TRUE
            "#,
        )
        .bind(projector_event_id)
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);

        if !inserted {
            tx.commit().await?;
            return Ok(());
        }

        let order_row = sqlx::query(
            r#"
            SELECT instrument_id
            FROM orders
            WHERE order_id = $1
            "#,
        )
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(order_row) = order_row else {
            tx.commit().await?;
            return Ok(());
        };
        let instrument_id: i64 = order_row.try_get("instrument_id")?;

        let existing_position = sqlx::query(
            r#"
            SELECT quantity::double precision AS quantity,
                   avg_cost_basis::double precision AS avg_cost_basis,
                   realized_pnl::double precision AS realized_pnl
            FROM portfolio_position
            WHERE account_id = $1
              AND connector_id = $2
              AND instrument_id = $3
            FOR UPDATE
            "#,
        )
        .bind(account_id)
        .bind(connector_id)
        .bind(instrument_id)
        .fetch_optional(&mut *tx)
        .await?;

        let (old_qty, old_avg, old_realized) = if let Some(row) = existing_position {
            (
                row.try_get::<f64, _>("quantity")?,
                row.try_get::<f64, _>("avg_cost_basis")?,
                row.try_get::<f64, _>("realized_pnl")?,
            )
        } else {
            (0.0_f64, 0.0_f64, 0.0_f64)
        };

        let (new_qty, new_avg, new_realized) = if side == "buy" {
            let qty = old_qty + quantity;
            let avg = if qty.abs() < f64::EPSILON {
                0.0
            } else {
                ((old_qty * old_avg) + (quantity * fill_price)) / qty
            };
            (qty, avg, old_realized)
        } else {
            let qty = old_qty - quantity;
            let realized = old_realized + ((fill_price - old_avg) * quantity);
            let avg = if qty.abs() < f64::EPSILON {
                0.0
            } else {
                old_avg
            };
            (qty, avg, realized)
        };

        sqlx::query(
            r#"
            INSERT INTO portfolio_position (
                account_id,
                connector_id,
                instrument_id,
                quantity,
                avg_cost_basis,
                realized_pnl,
                last_known_price,
                opened_at,
                last_updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)
            ON CONFLICT (account_id, connector_id, instrument_id)
            DO UPDATE SET
                quantity = EXCLUDED.quantity,
                avg_cost_basis = EXCLUDED.avg_cost_basis,
                realized_pnl = EXCLUDED.realized_pnl,
                last_known_price = EXCLUDED.last_known_price,
                last_updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(account_id)
        .bind(connector_id)
        .bind(instrument_id)
        .bind(new_qty)
        .bind(new_avg)
        .bind(new_realized)
        .bind(fill_price)
        .execute(&mut *tx)
        .await?;

        let currency_row = sqlx::query(
            r#"
            SELECT base_currency
            FROM connector
            WHERE connector_id = $1
            "#,
        )
        .bind(connector_id)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(currency_row) = currency_row {
            let currency: String = currency_row.try_get("base_currency")?;
            let notional = quantity * fill_price;
            let cash_delta = if side == "buy" { -notional } else { notional };

            sqlx::query(
                r#"
                INSERT INTO cash_balance (
                    account_id,
                    connector_id,
                    currency,
                    available,
                    reserved,
                    last_synced_at
                )
                VALUES ($1, $2, $3, $4, 0, CURRENT_TIMESTAMP)
                ON CONFLICT (account_id, connector_id, currency)
                DO UPDATE SET
                    available = cash_balance.available + EXCLUDED.available,
                    last_synced_at = CURRENT_TIMESTAMP
                "#,
            )
            .bind(account_id)
            .bind(connector_id)
            .bind(currency)
            .bind(cash_delta)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }
}
