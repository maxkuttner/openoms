# Kafka Bridge as Outbox (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Kafka receives every committed order event, plus fills and position changes, from a durable outbox. An outage leaves nothing missing. This replaces today's fire-and-forget `publish_events` calls.

**Architecture:**
- **Outbox capture:** an `AFTER INSERT` trigger on `order_event` writes the event's `global_position` into a new `kafka_outbox` table, inside the same transaction. A committed event is therefore always queued, and a rolled-back one never is.
- **Bridge task:** it wakes on the bus's order lane (phase 1), reads the outbox oldest-first, publishes each event to its topics and waits for the broker's ack, then deletes the acked rows.
- **Failure handling:** on a Kafka failure the bridge stops at that event, keeps it queued, and retries on a timer.
- **Removed:** the direct publish calls in `handlers.rs` and `execution.rs`, and the `kafka` parameter threaded through every stream and recon path.

**Tech Stack:** Rust, tokio, sqlx/Postgres, rdkafka 0.34 (`FutureProducer`).

**Spec:** `docs/superpowers/specs/2026-11-04-target-architecture-design.md` on branch `docs/target-architecture`: §4.1 (Kafka bridge row, laggard catch-up), §4.5 (topics), §6 (outbox decision), §7 phase 2.

## Global Constraints

- **Branch:** `feat/kafka-outbox`, created from `feat/event-bus`. Merge order: coinbase → okx → event-bus → kafka-outbox.
- **Kafka stays optional (spec §3):** without `KAFKA_BROKER` the OMS runs fully and no bridge starts. Outbox rows still accumulate (one small row per event); boot logs the pending count.
- **Kafka cannot slow or break the OMS (spec §2 goal 5):** nothing on the order path awaits Kafka. Only the bridge task talks to the broker.
- **Topics (spec §4.5):** orders go to the existing `KAFKA_TOPIC` (default topic name unchanged). Add fills on `KAFKA_FILLS_TOPIC`, default `oms.fills`, and positions on `KAFKA_POSITIONS_TOPIC`, default `oms.positions`.
- **Payload compatibility:** the orders topic message is the stored `payload_json`, verbatim. It is the same serialized `OrderDomainEvent` that `publish_events` sends today, keyed by `order_id`.
- **Delivery:** at least once. A crash between the broker's ack and the outbox delete re-sends the event. Consumers dedupe by `event_id`; a position message is a full snapshot and safe to repeat.
- **Constants (exact):**
  - `BATCH_SIZE = 500`;
  - `RETRY_SECS = 5` (after a failure);
  - `IDLE_RECHECK_SECS = 30` (backstop when the order lane is quiet);
  - producer `message.timeout.ms = 30000`.
- **Migration:** `db/migrations/ods/oms/0033_CREATE_KAFKA_OUTBOX.sql`, applied with `./target/debug/oms database migrate`. Existing events are **not** backfilled; the outbox starts empty.
- **DB tests:** mark them `#[ignore = "needs a live Postgres; run with --ignored"]`. They run against the shared dev DB. Never create databases. Each test deletes the outbox rows it created (`order_event` rows stay; the log is append-only).
- **Don't commit the user's files:** `.gitignore`, `.ignore` and `docs/superpowers/plans/2026-09-28-options-search-chain-strategy.md`. Stage files explicitly; never use `git commit -a`.
- **Out of scope:**
  - `oms.marks`: spec §4.5 marks it optional and off by default.
  - Inbound topics (`strategy.<id>.*`): phase 8.
  - The unused `create_projector_consumer`: it stays as it is.
- **My call, not the spec's:** the spec says the bridge "tracks the last published `global_position`". Phase 1's review showed why that can't work: `global_position` is assigned at insert, not at commit, so a cursor skips a lower position that commits later. A row-per-event outbox table has no such gap. It still publishes in `global_position` order, and the spec's success test (nothing missing after an outage) is what this plan proves.
- **My call:** a `oms.positions` message is the position row's state when the bridge publishes it, not at the fill. On catch-up, several fills for one position all carry the latest state. Messages are keyed `<portfolio_id>:<instrument_id>`, so a compacted topic ends at the correct value.

## Review Focus

1. **Kafka configured but down (today's dev setup: `localhost:9092` with no broker).** Orders and fills must work normally, and the log must say "down" once per outage and "recovered" once, not once per retry. Covered by Task 3 `outage_log_reports_each_transition_once`.
2. **An event's second record fails** (orders sent, fills refused). The event stays queued and is re-sent whole, never half-acked. Covered by Task 3 `partially_sent_event_is_not_acked`.
3. **A rolled-back append** (version conflict) must leave no outbox row. Covered by Task 1 `rolled_back_append_queues_nothing`.
4. **The outbox holds more than one batch after a long outage.** The bridge must loop until it is empty, not stop after 500. Covered by Task 4 `kafka_down_then_up_publishes_everything_in_order`, with batch size 2.
5. **A non-fill event** (routed, canceled) must produce exactly one record, on the orders topic only. Covered by Task 2 `routed_event_goes_to_orders_only`.

---

### Task 1: Outbox table, trigger and store queries

**Files:**
- Create: `db/migrations/ods/oms/0033_CREATE_KAFKA_OUTBOX.sql`
- Create: `src/kafka_outbox.rs`. It holds the DB access only; Task 3 adds the bridge.
- Modify: `src/main.rs` (`mod kafka_outbox;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct PositionSnapshot { pub net_qty: f64, pub avg_cost: f64, pub realized_pnl: f64 }
  pub struct OutboxRow {
      pub global_position: i64,
      pub order_id: Uuid,
      pub event_id: Uuid,
      pub event_type: String,
      pub payload: serde_json::Value,          // order_event.payload_json, verbatim
      pub portfolio_id: Option<Uuid>,          // from order_state
      pub instrument_id: Option<String>,
      pub side: Option<String>,
      pub position: Option<PositionSnapshot>,  // current position row for (portfolio, instrument)
  }
  pub async fn fetch_batch(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error>; // ORDER BY global_position
  pub async fn ack(pool: &PgPool, positions: &[i64]) -> Result<u64, sqlx::Error>;              // DELETE … = ANY($1)
  pub async fn pending_count(pool: &PgPool) -> Result<i64, sqlx::Error>;
  ```

- [ ] **Step 1: Write the migration.**
  - Table `kafka_outbox (global_position BIGINT PRIMARY KEY REFERENCES order_event(global_position), queued_at TIMESTAMPTZ NOT NULL DEFAULT now())`.
  - Function `order_event_enqueue_outbox()`, which inserts `NEW.global_position`.
  - Trigger `order_event_outbox AFTER INSERT ON order_event FOR EACH ROW`.
  - A header comment on why it is a row-per-event table and not a cursor (see Global Constraints).
- [ ] **Step 2: Apply it.** `cargo build && ./target/debug/oms database migrate` reports 0033 as applied.
- [ ] **Step 3: Write the failing DB tests** in `kafka_outbox.rs` `mod tests`, reusing `handlers::tests::{test_pool, seed_*}` and `OrderEventStore`:
  - `appended_event_is_queued_with_its_order_context`:
    1. Seed an order (as in `execution.rs` tests) and append one `order_routed` event.
    2. `fetch_batch` contains a row with that `global_position`, `event_type == "order_routed"`, `portfolio_id == Some(seeded portfolio)` and `payload` equal to what was appended.
    3. `ack(&[pos])` returns 1.
    4. Clean up the seeded rows.
  - `rolled_back_append_queues_nothing`: begin a tx, `append_events_in_tx`, roll back. `fetch_batch(pool, 10_000)` has no row for that order.
  - `ack_removes_only_named_rows`: append 2 events and `ack` the first. `fetch_batch` still has the second; then ack it.
- [ ] **Step 4: Run, expect FAIL.** `cargo test --bin oms -- --ignored kafka_outbox` fails: `fetch_batch` not found.
- [ ] **Step 5: Implement.** One query: `kafka_outbox JOIN order_event USING (global_position)`, `LEFT JOIN order_state` on `order_id`, `LEFT JOIN position` on `(portfolio_id, instrument_id)`, numerics cast `::double precision`.
- [ ] **Step 6: Run.** The `--ignored kafka_outbox` tests pass 3/3; `cargo test --bin oms` stays green.
- [ ] **Step 7: Commit** the migration, `src/kafka_outbox.rs` and `src/main.rs`: `feat(kafka): durable outbox queued by an order_event trigger`.

### Task 2: Map an outbox row to Kafka records

**Files:**
- Modify: `src/kafka_outbox.rs`

**Interfaces:**
- Consumes: Task 1 `OutboxRow`, `PositionSnapshot`.
- Produces:
  ```rust
  #[derive(Clone)] pub struct OutboxTopics { pub orders: String, pub fills: String, pub positions: String }
  #[derive(Debug, Clone, PartialEq)] pub struct OutboxRecord { pub topic: String, pub key: String, pub payload: String }
  pub fn records_for(row: &OutboxRow, topics: &OutboxTopics) -> Vec<OutboxRecord>;
  ```
  - Every row produces one orders record: key `order_id`, payload `row.payload` serialized.
  - `order_partially_filled` / `order_filled` rows also produce:
    - one fills record: key `order_id`, payload `{"event": <payload>, "portfolio_id", "instrument_id", "side"}`;
    - when `position` is `Some`, one positions record: key `"<portfolio_id>:<instrument_id>"`, payload `{"portfolio_id", "instrument_id", "net_qty", "avg_cost", "realized_pnl", "global_position"}`.
  - Order of the returned records: orders, fills, positions.

- [ ] **Step 1: Write the failing unit tests:**
  - `routed_event_goes_to_orders_only`: a row with `event_type "order_routed"` yields exactly 1 record, on `topics.orders`, with key = `order_id`, and its payload parses back to `row.payload`.
  - `fill_fans_out_to_orders_fills_positions`: an `order_filled` row with a position yields 3 records with topics `[orders, fills, positions]`. The fills payload `["portfolio_id"]` matches. The positions key is `"<p>:<i>"` and its `["net_qty"]` matches.
  - `fill_without_position_row_skips_positions`: `position: None` yields 2 records.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms kafka_outbox` fails: `records_for` not found.
- [ ] **Step 3: Implement** with `serde_json::json!`.
- [ ] **Step 4: Run.** `cargo test --bin oms kafka_outbox` passes 3/3.
- [ ] **Step 5: Commit** `src/kafka_outbox.rs`: `feat(kafka): map outbox events to orders, fills and positions records`.

### Task 3: Delivery and outage logging

**Files:**
- Modify: `src/kafka_outbox.rs`

**Interfaces:**
- Consumes: Task 2 `OutboxRecord`, `records_for`.
- Produces:
  ```rust
  #[async_trait::async_trait]
  pub trait OutboxSink: Send + Sync {
      /// Publish one record; Ok only once the broker has acked it.
      async fn send(&self, record: &OutboxRecord) -> Result<(), String>;
  }
  /// Send events in order; stop at the first failure. Returns the global positions whose
  /// records were ALL acked, plus the failure, if any.
  pub async fn deliver(batch: &[(i64, Vec<OutboxRecord>)], sink: &dyn OutboxSink) -> (Vec<i64>, Option<String>);

  #[derive(Default)] pub struct OutageLog { down_since: Option<std::time::Instant>, failed_attempts: u32 }
  #[derive(Debug, PartialEq)] pub enum Transition { WentDown, Recovered { failed_attempts: u32 } }
  impl OutageLog { pub fn on_attempt(&mut self, ok: bool) -> Option<Transition>; }
  ```
  Callers log a `Transition` (`warn!` on down, `info!` on recovered); `None` means stay quiet.

- [ ] **Step 1: Write the failing unit tests.** Use a test `FakeSink` with a `Mutex<Vec<OutboxRecord>>` of sent records and a `fail_from: Option<usize>`, which fails every send from that index on.
  - `all_acked_when_sink_is_up`: 3 events → acked `[1, 2, 3]`, error `None`, and the sink saw every record in order.
  - `stops_at_first_failed_event`: events 1, 2, 3, the sink fails on event 2's record → acked `[1]`, error `Some`, and event 3 was never sent.
  - `partially_sent_event_is_not_acked`: event 1 has 2 records and the sink fails on the second → acked `[]`.
  - `outage_log_reports_each_transition_once`: the sequence `ok, fail, fail, fail, ok, ok` yields `None, WentDown, None, None, Recovered{failed_attempts: 3}, None`.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms kafka_outbox` fails: `deliver` not found.
- [ ] **Step 3: Implement** `deliver` (sequential awaits; no concurrency) and `OutageLog`.
- [ ] **Step 4: Run.** `cargo test --bin oms kafka_outbox` is green.
- [ ] **Step 5: Commit** `src/kafka_outbox.rs`: `feat(kafka): ordered at-least-once delivery with outage logging`.

### Task 4: Bridge loop, Kafka sink and boot wiring

**Files:**
- Modify: `src/kafka_outbox.rs` (`drain`, `run`)
- Modify: `src/kafka.rs`:
  - `KafkaConfig` gains `fills_topic` and `positions_topic` (env plus defaults) and `pub fn topics(&self) -> OutboxTopics`;
  - the producer sets `message.timeout.ms = 30000`;
  - add `pub struct KafkaSink(pub KafkaClient)` with `impl OutboxSink` (`producer.send(record, Timeout::Never).await`; the broker timeout bounds it).
- Modify: `src/main.rs`:
  - keep the `KafkaConfig` for its topics;
  - if Kafka is configured, `tokio::spawn(kafka_outbox::run(pool, Arc::new(KafkaSink(client)), state.event_bus().subscribe_order(), cfg.topics()))`;
  - otherwise log `kafka outbox: {n} events waiting; Kafka not configured` when `n > 0`.

**Interfaces:**
- Consumes: Tasks 1–3, plus phase 1 `EventBus::subscribe_order`, `OrderLaneEvent`.
- Produces:
  ```rust
  /// Publish until the outbox is empty or the sink fails. Returns events published.
  pub async fn drain(pool: &PgPool, sink: &dyn OutboxSink, topics: &OutboxTopics, batch_size: i64) -> Result<usize, DrainError>;
  pub enum DrainError { Db(sqlx::Error), Sink { published: usize, error: String } }
  pub async fn run(pool: PgPool, sink: Arc<dyn OutboxSink>, order_rx: broadcast::Receiver<OrderLaneEvent>, topics: OutboxTopics) -> !;
  ```
- `run` behaviour:
  - Drain, then feed the outcome to `OutageLog`.
  - On `Sink`: sleep `RETRY_SECS`.
  - On success: wait for any order-lane receive (including `Lagged`), or `IDLE_RECHECK_SECS`. On `Closed`, use the timer only.
  - On `Db`: `error!`, then sleep `RETRY_SECS`.

- [ ] **Step 1: Write the failing DB test** `kafka_down_then_up_publishes_everything_in_order` (ignored):
  1. Seed one order and append 5 events.
  2. Use a `FakeSink` that fails from its 3rd send. `drain(.., batch_size = 2)` returns `Err(Sink{published: 2, ..})`.
  3. Switch the sink to "up". `drain` returns `Ok(3)`.
  4. Across both drains, the sink's orders-topic records for this order are exactly the 5 `event_id`s, in append order.
  5. `fetch_batch` has no row for the order. Clean up.

  Filter the sink's records by key = this `order_id`: other pending dev-DB events may be in the outbox too.
- [ ] **Step 2: Run, expect FAIL.** `cargo test --bin oms -- --ignored kafka_down_then_up` fails: `drain` not found.
- [ ] **Step 3: Implement** `drain`, `run`, `KafkaSink`, the config fields and the `main.rs` wiring.
- [ ] **Step 4: Run.**
  - The DB test passes; `cargo test --bin oms` is green.
  - Boot check, with the current `.env` (Kafka at `localhost:9092`, no broker):
    1. `cargo build && ./target/debug/oms`;
    2. place one paper order from the cockpit;
    3. the order completes;
    4. the log shows exactly one `kafka outbox: Kafka down` line, not one per retry.
- [ ] **Step 5: Commit** `src/kafka_outbox.rs`, `src/kafka.rs` and `src/main.rs`: `feat(kafka): outbox bridge publishes from the event store, catching up after outages`.

### Task 5: Remove the direct publish path and the threaded `kafka` parameter

**Files:**
- Modify:
  - `src/kafka.rs`: delete `publish_events`;
  - `src/handlers.rs`: delete the 3 calls and the import;
  - `src/execution.rs`: delete the call, and the `kafka` parameter of `process_execution_report` / `apply_once`.
- Modify (mechanical: drop the `kafka: &Option<KafkaClient>` / `Option<KafkaClient>` parameter and fields, and every argument passing them):
  - `src/recon_orders.rs`;
  - the alpaca, binance, bybit, coinbase and okx `_stream.rs` files;
  - `src/fix/mod.rs`;
  - `src/reload.rs` (`RegistrationDeps.kafka`);
  - `src/app_state.rs` (`kafka` field, `kafka()`, the `AppState::new` parameter);
  - `src/admin.rs`, `src/main.rs`, `src/reload_tests.rs`;
  - test fixtures calling `AppState::new` in `handlers.rs` and `admin.rs`.
- Leave `src/setup/status.rs` alone: it reports Kafka configuration and stays.

- [ ] **Step 1: Run** `grep -rn "publish_events\|kafka" src | grep -v "src/kafka.rs\|src/kafka_outbox.rs\|src/setup/status.rs\|src/main.rs"`. Expected: about 90 hits, the work list.
- [ ] **Step 2: Remove** each item. `main.rs` keeps the Kafka config and client only to start the bridge.
- [ ] **Step 3: Run.**
  - `cargo build` produces no new warnings.
  - `cargo test --bin oms` is green.
  - `cargo test --bin oms -- --ignored execution::tests kafka_outbox` passes.
  - The grep from Step 1 returns nothing.
- [ ] **Step 4: Update docs.** Find every user-facing page with `grep -rln "KAFKA_TOPIC\|KAFKA_BROKER" docs site README.md`. On each one:
  - add `KAFKA_FILLS_TOPIC` and `KAFKA_POSITIONS_TOPIC`;
  - state that delivery is at least once (dedupe by `event_id`);
  - say that `oms.positions` is a keyed snapshot.
- [ ] **Step 5: Commit** the modified files by name: `refactor(kafka): drop the direct publish path; connectors no longer carry a Kafka client`.
