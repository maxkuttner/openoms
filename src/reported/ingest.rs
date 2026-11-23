//! Reported fill ingest: one batch from a reported strategy, booked fill by fill
//! into the strategy's portfolio. Idempotent per batch (`batch_id`: a replay
//! returns the stored result) and per fill (`venue + account_ref + exec_id`). A
//! fill that cannot be booked becomes a REPORTED recon break; it never rejects
//! the batch.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::orders::state::OrderSide;
use crate::fees::FeeEffect;
use crate::positions;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_BATCH_FILLS: usize = 500;

/// One POSTed batch. Fills stay raw so a malformed one becomes a break, not a 400.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FillBatch {
    pub schema_version: u32,
    pub batch_id: String,
    #[serde(default)]
    pub sequence: Option<i64>,
    pub fills: Vec<Value>,
}

/// One reported fill. Unknown fields (any portfolio/book included) are ignored:
/// the strategy books only into its own portfolio.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReportedFill {
    pub venue: String,
    pub account_ref: String,
    pub exec_id: String,
    #[serde(default)]
    pub client_order_id: Option<String>,
    pub symbol: String,
    #[serde(deserialize_with = "side_any_case")]
    pub side: OrderSide,
    pub qty: f64,
    pub price: f64,
    #[serde(default)]
    pub fee: Option<f64>,
    #[serde(default)]
    pub fee_asset: Option<String>,
    pub trade_time: DateTime<Utc>,
}

fn side_any_case<'de, D: Deserializer<'de>>(d: D) -> Result<OrderSide, D::Error> {
    let s = String::deserialize(d)?;
    match s.to_ascii_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err(serde::de::Error::custom(format!("side must be BUY or SELL, got {s:?}"))),
    }
}

impl ReportedFill {
    /// Shape checks serde can't express.
    fn validate(&self) -> Result<(), String> {
        for (name, v) in [("venue", &self.venue), ("account_ref", &self.account_ref), ("exec_id", &self.exec_id), ("symbol", &self.symbol)] {
            if v.trim().is_empty() {
                return Err(format!("{name} is empty"));
            }
        }
        if !(self.qty.is_finite() && self.qty > 0.0) {
            return Err("qty must be > 0".into());
        }
        if !(self.price.is_finite() && self.price > 0.0) {
            return Err("price must be > 0".into());
        }
        match (self.fee, &self.fee_asset) {
            (Some(f), _) if !f.is_finite() || f < 0.0 => Err("fee must be >= 0".into()),
            (Some(_), None) => Err("fee without fee_asset".into()),
            _ => Ok(()),
        }
    }

    /// The position-ordering execution id: `venue|account_ref|exec_id`, so two
    /// venues reusing one exec id at one trade time still sort deterministically.
    /// `replay_sql` emits the same expression for reported rows.
    pub(crate) fn order_exec_id(&self) -> String {
        format!("{}|{}|{}", self.venue, self.account_ref, self.exec_id)
    }

    fn side_db(&self) -> &'static str {
        match self.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Booked,
    Duplicate,
    Break,
}

#[cfg(test)]
impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Booked => "booked",
            Outcome::Duplicate => "duplicate",
            Outcome::Break => "break",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillOutcome {
    pub exec_id: Option<String>,
    pub outcome: Outcome,
    pub break_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub batch_id: String,
    pub results: Vec<FillOutcome>,
    pub sequence_gap: bool,
}

#[derive(Debug)]
pub enum IngestError {
    SchemaVersion(u32),
    TooLarge(usize),
    UnknownStrategy,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for IngestError {
    fn from(e: sqlx::Error) -> Self {
        IngestError::Db(e)
    }
}

pub const UNKNOWN_INSTRUMENT: &str = "UNKNOWN_INSTRUMENT";
pub const INVALID_FILL: &str = "INVALID_FILL";
pub const CONFLICTING_DUPLICATE: &str = "CONFLICTING_DUPLICATE";

/// Ingest one batch for `strategy_code` (already authenticated).
pub async fn ingest(pool: &PgPool, strategy_code: &str, batch: FillBatch) -> Result<BatchResult, IngestError> {
    if batch.schema_version != SCHEMA_VERSION {
        return Err(IngestError::SchemaVersion(batch.schema_version));
    }
    if batch.fills.len() > MAX_BATCH_FILLS {
        return Err(IngestError::TooLarge(batch.fills.len()));
    }
    let mut tx = pool.begin().await?;
    // The batch row is the lock: a concurrent insert of the same batch waits for
    // this transaction, then conflicts and replays the stored result.
    let prev_seq: Option<i64> =
        sqlx::query_scalar("SELECT max(sequence) FROM reported_batch WHERE strategy_code = $1")
            .bind(strategy_code)
            .fetch_one(&mut *tx)
            .await?;
    let payload = serde_json::to_value(&batch).unwrap_or(Value::Null);
    let inserted = sqlx::query(
        "INSERT INTO reported_batch (strategy_code, batch_id, sequence, response_json, payload_json) \
         VALUES ($1, $2, $3, '{}', $4) ON CONFLICT DO NOTHING",
    )
    .bind(strategy_code)
    .bind(&batch.batch_id)
    .bind(batch.sequence)
    .bind(&payload)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        drop(tx);
        let stored: Value = sqlx::query_scalar(
            "SELECT response_json FROM reported_batch WHERE strategy_code = $1 AND batch_id = $2",
        )
        .bind(strategy_code)
        .bind(&batch.batch_id)
        .fetch_one(pool)
        .await?;
        return serde_json::from_value(stored).map_err(|e| IngestError::Db(sqlx::Error::Decode(Box::new(e))));
    }

    let portfolio_id: Uuid = sqlx::query_scalar("SELECT portfolio_id FROM strategy WHERE code = $1")
        .bind(strategy_code)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IngestError::UnknownStrategy)?;

    let sequence_gap = match (batch.sequence, prev_seq) {
        (Some(s), Some(p)) => s != p + 1,
        _ => false,
    };
    if sequence_gap {
        tracing::warn!(strategy = strategy_code, batch_id = %batch.batch_id, sequence = ?batch.sequence,
            previous = ?prev_seq, "reported batch sequence gap");
    }

    let mut results = Vec::with_capacity(batch.fills.len());
    for raw in &batch.fills {
        let mut sp = sqlx::Connection::begin(&mut *tx).await?; // savepoint per fill
        let outcome = ingest_one(&mut sp, strategy_code, portfolio_id, &batch.batch_id, raw).await?;
        sp.commit().await?;
        results.push(outcome);
    }

    let n = |o: Outcome| results.iter().filter(|r| r.outcome == o).count() as i32;
    let result = BatchResult { batch_id: batch.batch_id.clone(), results: results.clone(), sequence_gap };
    sqlx::query(
        "UPDATE reported_batch SET booked = $3, duplicate = $4, broken = $5, sequence_gap = $6, response_json = $7 \
         WHERE strategy_code = $1 AND batch_id = $2",
    )
    .bind(strategy_code)
    .bind(&batch.batch_id)
    .bind(n(Outcome::Booked))
    .bind(n(Outcome::Duplicate))
    .bind(n(Outcome::Break))
    .bind(sequence_gap)
    .bind(serde_json::to_value(&result).unwrap_or(Value::Null))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    // Last-report time is MAX(reported_batch.received_at): nothing else to write.
    Ok(result)
}

/// What fee classification needs about the fill's instrument on its venue.
struct FeeContext {
    currency: String,
    asset_class: String,
    native_id: Option<String>,
}

async fn ingest_one(
    conn: &mut PgConnection,
    strategy_code: &str,
    portfolio_id: Uuid,
    batch_id: &str,
    raw: &Value,
) -> Result<FillOutcome, sqlx::Error> {
    let exec_id = raw.get("exec_id").and_then(Value::as_str).map(str::to_string);
    let broke = |id| FillOutcome { exec_id: exec_id.clone(), outcome: Outcome::Break, break_id: Some(id) };
    let fill: ReportedFill = match serde_json::from_value::<ReportedFill>(raw.clone()) {
        Ok(f) => match f.validate() {
            Ok(()) => f,
            Err(why) => {
                tracing::warn!(strategy = strategy_code, ?exec_id, why, "invalid reported fill");
                return Ok(broke(open_reported_break(conn, strategy_code, INVALID_FILL, raw).await?));
            }
        },
        Err(e) => {
            tracing::warn!(strategy = strategy_code, ?exec_id, error = %e, "malformed reported fill");
            return Ok(broke(open_reported_break(conn, strategy_code, INVALID_FILL, raw).await?));
        }
    };
    let found: Vec<i64> = sqlx::query_scalar(
        "SELECT instrument_id FROM broker_instrument WHERE broker_code = $1 AND broker_symbol = $2",
    )
    .bind(&fill.venue)
    .bind(&fill.symbol)
    .fetch_all(&mut *conn)
    .await?;
    let [instrument_id] = found[..] else {
        return Ok(broke(open_reported_break(conn, strategy_code, UNKNOWN_INSTRUMENT, raw).await?));
    };
    book_fill(conn, strategy_code, portfolio_id, instrument_id, &fill, batch_id, raw).await
}

/// Fee asset → effect: the instrument currency is Quote, its base is Base,
/// anything else Other (tracked, no position effect) with an alarm.
fn classify_fee(strategy_code: &str, fill: &ReportedFill, r: &FeeContext, asset: &str) -> FeeEffect {
    if r.native_id.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(asset)) && !asset.eq_ignore_ascii_case(&r.currency) {
        return FeeEffect::Base;
    }
    let effect = crate::fees::fee_effect(asset, &fill.symbol, &r.asset_class, &r.currency);
    if effect == FeeEffect::Other {
        tracing::warn!(strategy = strategy_code, exec_id = %fill.exec_id, fee_asset = asset,
            "reported fee in a third asset: booked with no position effect");
    }
    effect
}

fn effect_db(e: FeeEffect) -> &'static str {
    match e {
        FeeEffect::Quote => "Quote",
        FeeEffect::Base => "Base",
        FeeEffect::Other => "Other",
    }
}

/// Insert one resolved fill and apply it (and its fee) to the strategy's
/// position. An existing identical fill is a duplicate; a differing one is a
/// `CONFLICTING_DUPLICATE` break. Reused by break adoption.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn book_fill(
    conn: &mut PgConnection,
    strategy_code: &str,
    portfolio_id: Uuid,
    instrument_id: i64,
    fill: &ReportedFill,
    batch_id: &str,
    raw: &Value,
) -> Result<FillOutcome, sqlx::Error> {
    let exec_id = Some(fill.exec_id.clone());
    let fee_effect = match (fill.fee, fill.fee_asset.as_deref()) {
        (Some(_), Some(asset)) => {
            let (currency, asset_class, native_id): (String, String, Option<String>) = sqlx::query_as(
                "SELECT i.currency, i.asset_class, bi.native_id FROM instrument i \
                   LEFT JOIN broker_instrument bi ON bi.instrument_id = i.id AND bi.broker_code = $2 \
                  WHERE i.id = $1",
            )
            .bind(instrument_id)
            .bind(&fill.venue)
            .fetch_one(&mut *conn)
            .await?;
            Some(classify_fee(strategy_code, fill, &FeeContext { currency, asset_class, native_id }, asset))
        }
        _ => None,
    };
    let inserted = sqlx::query(
        "INSERT INTO reported_fill (id, strategy_code, batch_id, venue, account_ref, exec_id, client_order_id, \
           symbol, instrument_id, side, qty, price, fee, fee_asset, fee_effect, trade_time, raw_json) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17) \
         ON CONFLICT (strategy_code, venue, account_ref, exec_id) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(strategy_code)
    .bind(batch_id)
    .bind(&fill.venue)
    .bind(&fill.account_ref)
    .bind(&fill.exec_id)
    .bind(&fill.client_order_id)
    .bind(&fill.symbol)
    .bind(instrument_id)
    .bind(fill.side_db())
    .bind(fill.qty)
    .bind(fill.price)
    .bind(fill.fee)
    .bind(&fill.fee_asset)
    .bind(fee_effect.map(effect_db))
    .bind(fill.trade_time)
    .bind(raw)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        let same: bool = sqlx::query_scalar(
            "SELECT symbol = $5 AND instrument_id = $6 AND side = $7 AND qty = $8::float8::numeric \
                    AND price = $9::float8::numeric AND fee IS NOT DISTINCT FROM $10::float8::numeric \
                    AND fee_asset IS NOT DISTINCT FROM $11 AND trade_time = $12 \
               FROM reported_fill WHERE strategy_code = $1 AND venue = $2 AND account_ref = $3 AND exec_id = $4",
        )
        .bind(strategy_code)
        .bind(&fill.venue)
        .bind(&fill.account_ref)
        .bind(&fill.exec_id)
        .bind(&fill.symbol)
        .bind(instrument_id)
        .bind(fill.side_db())
        .bind(fill.qty)
        .bind(fill.price)
        .bind(fill.fee)
        .bind(&fill.fee_asset)
        .bind(fill.trade_time)
        .fetch_one(&mut *conn)
        .await?;
        if same {
            return Ok(FillOutcome { exec_id, outcome: Outcome::Duplicate, break_id: None });
        }
        let id = open_reported_break(conn, strategy_code, CONFLICTING_DUPLICATE, raw).await?;
        return Ok(FillOutcome { exec_id, outcome: Outcome::Break, break_id: Some(id) });
    }

    let instrument_key = instrument_id.to_string();
    let order_exec = fill.order_exec_id();
    let rebuilt = positions::persist_fill(
        conn, portfolio_id, &instrument_key, fill.side, fill.qty, fill.price, fill.trade_time, &order_exec,
    )
    .await?;
    // A rebuild replays every reported_fill row present, this fill's fee included
    // (the row carries it), so the fee must not be applied a second time. Later
    // fills of the batch are not inserted yet and still apply normally.
    if let (false, Some(amount), Some(effect)) = (rebuilt, fill.fee, fee_effect) {
        positions::persist_reported_fee(conn, portfolio_id, &instrument_key, &order_exec, fill.trade_time, amount, effect)
            .await?;
    }
    Ok(FillOutcome { exec_id, outcome: Outcome::Booked, break_id: None })
}

/// Open (or find the open) REPORTED break for this raw fill. One open break per
/// `(strategy, venue, account_ref, exec_id)`, so a retried fill reuses it.
pub(crate) async fn open_reported_break(
    conn: &mut PgConnection,
    strategy_code: &str,
    kind: &str,
    raw: &Value,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO recon_break (id, kind, source, strategy_code, reported_fill_json) \
         VALUES ($1, $2, 'REPORTED', $3, $4) \
         ON CONFLICT (strategy_code, (reported_fill_json->>'venue'), (reported_fill_json->>'account_ref'), \
                      (reported_fill_json->>'exec_id')) \
           WHERE status = 'open' AND source = 'REPORTED' \
         DO UPDATE SET reported_fill_json = recon_break.reported_fill_json \
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(kind)
    .bind(strategy_code)
    .bind(raw)
    .fetch_one(conn)
    .await
}
