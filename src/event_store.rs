use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

/*
 * The classes in this file are merely for database interaction.
 * Therefore, they are not part of the domain/orders.
 */

#[derive(Debug, Clone)]
pub struct OrderEventRecord {
    pub global_position: i64,
    pub order_id: Uuid,
    pub version: i64,
    pub event_id: String,
    pub event_type: String,
    pub actor: String,
    pub payload: Value,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub schema_version: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewOrderEvent {
    pub event_id: String,
    pub event_type: String,
    pub actor: String,
    pub payload: Value,
    pub correlation_id: Option<String>,
    pub causation_id: Option<String>,
    pub schema_version: i32,
}

#[derive(Debug, Clone)]
pub struct AppendResult {
    pub last_version: i64,
    pub appended: usize,
}

#[derive(Debug, Clone)]
pub struct ConcurrencyError {
    pub expected: i64,
    pub actual: i64,
}

#[derive(Debug)]
pub enum EventStoreError {
    Sqlx(sqlx::Error),
    Concurrency(ConcurrencyError),
}

impl From<sqlx::Error> for EventStoreError {
    fn from(value: sqlx::Error) -> Self {
        Self::Sqlx(value)
    }
}



// Order event store to append event logs to a database
#[derive(Clone)]
pub struct OrderEventStore {
    pool: PgPool,
}

impl OrderEventStore {

    // create new event store
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }


    // fetch all events for a given order_id
    pub async fn load_stream(&self, order_id: Uuid) -> Result<Vec<OrderEventRecord>, EventStoreError> {
        let rows = sqlx::query(
            "SELECT
                global_position,
                order_id,
                version,
                event_id::text AS event_id,
                event_type,
                actor,
                payload_json,
                correlation_id::text AS correlation_id,
                causation_id::text AS causation_id,
                schema_version,
                created_at
             FROM order_event
             WHERE order_id = $1
             ORDER BY version ASC"
        )
        .bind(order_id)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            events.push(OrderEventRecord {
                global_position: row.get::<i64, _>("global_position"),
                order_id: row.get::<Uuid, _>("order_id"),
                version: row.get::<i64, _>("version"),
                event_id: row.get::<String, _>("event_id"),
                event_type: row.get::<String, _>("event_type"),
                actor: row.get::<String, _>("actor"),
                payload: row.get::<Value, _>("payload_json"),
                correlation_id: row.get::<Option<String>, _>("correlation_id"),
                causation_id: row.get::<Option<String>, _>("causation_id"),
                schema_version: row.get::<i32, _>("schema_version"),
                created_at: row.get::<DateTime<Utc>, _>("created_at"),
            });
        }

        Ok(events)
    }
    

    // append an order event
    pub async fn append_events(
        &self,
        order_id: Uuid,
        expected_version: i64,
        events: &[NewOrderEvent],
    ) -> Result<AppendResult, EventStoreError> {
        if events.is_empty() {
            return Ok(AppendResult {
                last_version: expected_version,
                appended: 0,
            });
        }

        let mut tx = self.pool.begin().await?;
        let result = self
            .append_events_in_tx(&mut tx, order_id, expected_version, events)
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn append_events_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        order_id: Uuid,
        expected_version: i64,
        events: &[NewOrderEvent],
    ) -> Result<AppendResult, EventStoreError> {
        if events.is_empty() {
            return Ok(AppendResult {
                last_version: expected_version,
                appended: 0,
            });
        }

        self.lock_order_stream(tx, order_id).await?;

        let actual_version = self.current_version(tx, order_id).await?;
        if actual_version != expected_version {
            return Err(EventStoreError::Concurrency(ConcurrencyError {
                expected: expected_version,
                actual: actual_version,
            }));
        }

        for (index, event) in events.iter().enumerate() {
            let version = expected_version + (index as i64) + 1;
            sqlx::query(
                "INSERT INTO order_event (
                    order_id,
                    version,
                    event_id,
                    event_type,
                    actor,
                    payload_json,
                    correlation_id,
                    causation_id,
                    schema_version
                 ) VALUES (
                    $1,
                    $2,
                    CAST($3 AS uuid),
                    $4,
                    $5,
                    $6,
                    CAST($7 AS uuid),
                    CAST($8 AS uuid),
                    $9
                 )"
            )
            .bind(order_id)
            .bind(version)
            .bind(&event.event_id)
            .bind(&event.event_type)
            .bind(&event.actor)
            .bind(&event.payload)
            .bind(&event.correlation_id)
            .bind(&event.causation_id)
            .bind(event.schema_version)
            .execute(&mut **tx)
            .await?;
        }

        Ok(AppendResult {
            last_version: expected_version + events.len() as i64,
            appended: events.len(),
        })
    }

    // function to lock all records with given order_id
    async fn lock_order_stream(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        order_id: Uuid,
    ) -> Result<(), EventStoreError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(order_id.to_string())
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    // function to return the current version given an order id and 
    // a mutable in-progress SQLx transaction
    async fn current_version(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        order_id: Uuid,
    ) -> Result<i64, EventStoreError> {
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(version), 0) FROM order_event WHERE order_id = $1"
        )
        .bind(order_id)
        .fetch_one(&mut **tx)
        .await?;
        Ok(version)
    }
}
