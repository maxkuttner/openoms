use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use gateway::adapter::BrokerOrderRequest;
use uuid::Uuid;
use serde::{Deserialize, Serialize};

use gateway::runtime::{
    AccountIdentity, AssetBalance, InstrumentRules, Operation, OrderType, RuntimeEvent,
    RuntimeOrder, RuntimeOrderState, Side, StreamState, TimeInForce, Trade, TradeKey, VenueEvent,
};

use crate::assets::{AssetLedger, Reservation};
use crate::RuntimeError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub instrument: String,
    pub side: Side,
    pub quantity: f64,
    pub order_type: OrderType,
    pub limit_price: Option<f64>,
    /// Required for market orders: the most quote currency this order may spend.
    pub max_quote_spend: Option<f64>,
    pub time_in_force: TimeInForce,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InstrumentPrices {
    /// Valuation for the candidate when it has no limit price.
    pub order: Option<f64>,
    /// Mark used by the price band and reducing checks.
    pub reference: Option<f64>,
    /// Valuation for the projected position; never replaced by an order price.
    pub position: Option<f64>,
    /// Monotonic receipt time. Admission never trusts a value from the future.
    pub received_at: Instant,
}

impl InstrumentPrices {
    pub fn new(order: Option<f64>, reference: Option<f64>, position: Option<f64>, received_at: Instant) -> Self {
        Self { order, reference, position, received_at }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prices {
    entries: BTreeMap<String, InstrumentPrices>,
}

impl Prices {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, instrument: impl Into<String>, prices: InstrumentPrices) {
        self.entries.insert(instrument.into(), prices);
    }

    pub fn get(&self, instrument: &str) -> Option<&InstrumentPrices> {
        self.entries.get(instrument)
    }
}

/// Identifies local acceptance. It is not venue acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub client_order_id: String,
    pub sequence: u64,
}

#[derive(Debug, Clone)]
pub struct PreparedOrder {
    pub submission: Submission,
    pub intent: Intent,
    pub broker_request: BrokerOrderRequest,
    pub reservation: Reservation,
    _sealed: (),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    pub client_order_id: String,
    pub venue_order_id: Option<String>,
    pub symbol: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum LocalState {
    Pending,
    Live,
    PartiallyFilled,
    CancelRequested,
    Filled,
    Canceled,
    Rejected,
    Expired,
    Unknown,
}

impl LocalState {
    fn is_working(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Live | Self::PartiallyFilled | Self::CancelRequested | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RestingOrder {
    client_order_id: String,
    venue_order_id: Option<String>,
    instrument: String,
    side: Side,
    original_quantity: f64,
    executed_quantity: f64,
    leaves_quantity: f64,
    limit_price: Option<f64>,
    state: LocalState,
    reservation_asset: String,
    reserved_per_unit: f64,
    reserved_amount: f64,
    /// Fills already counted for this order, so a checkpoint can carry them
    /// and a redelivered fill is not counted twice after a restart.
    #[serde(default)]
    trades: Vec<TradeKey>,
}

/// The book's durable state at one journal sequence: what a restart needs so
/// the records before it can be dropped. Finished orders are left out; their
/// effect lives on in `positions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookCheckpoint {
    pub positions: BTreeMap<String, f64>,
    pub asset_totals: BTreeMap<String, f64>,
    /// Client order id counter, so ids are never reused after a restart.
    pub sequence: u64,
    orders: Vec<RestingOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TradingState {
    Active,
    Reducing,
    Halted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BookHealth {
    Unknown,
    Live,
    Stale,
    Closed,
}

pub struct Book {
    identity: AccountIdentity,
    limits: BTreeMap<String, risk::Limits>,
    instruments: BTreeMap<String, InstrumentRules>,
    positions: BTreeMap<String, f64>,
    orders: BTreeMap<String, RestingOrder>,
    by_venue: HashMap<String, String>,
    assets: AssetLedger,
    state: TradingState,
    instrument_states: BTreeMap<String, TradingState>,
    allocation: Option<(String, f64, Option<f64>)>,
    max_price_age: Option<Duration>,
    fee_allowance_bps: f64,
    health: BookHealth,
    rate: VecDeque<Instant>,
    seen_trades: HashSet<TradeKey>,
    sequence: u64,
    pause_reason: Option<String>,
}

impl Book {
    pub fn new(
        identity: AccountIdentity,
        limits: BTreeMap<String, risk::Limits>,
    ) -> Result<Self, RuntimeError> {
        if identity.account_id.is_nil() {
            return Err(RuntimeError::InvalidConfig("account identity has no account id".into()));
        }
        if identity.venue.trim().is_empty()
            || identity.environment.trim().is_empty()
            || identity.external_account_ref.trim().is_empty()
        {
            return Err(RuntimeError::InvalidConfig(
                "venue, environment and external account reference are required".into(),
            ));
        }
        for (symbol, limits) in &limits {
            validate_limits(limits).map_err(|e| {
                RuntimeError::InvalidConfig(format!("{symbol}: {e}"))
            })?;
        }
        Ok(Self {
            identity,
            limits,
            instruments: BTreeMap::new(),
            positions: BTreeMap::new(),
            orders: BTreeMap::new(),
            by_venue: HashMap::new(),
            assets: AssetLedger::default(),
            state: TradingState::Active,
            instrument_states: BTreeMap::new(),
            allocation: None,
            max_price_age: None,
            fee_allowance_bps: 0.0,
            health: BookHealth::Unknown,
            rate: VecDeque::new(),
            seen_trades: HashSet::new(),
            sequence: 0,
            pause_reason: None,
        })
    }

    pub fn install_instruments(&mut self, rules: impl IntoIterator<Item = InstrumentRules>) -> Result<(), RuntimeError> {
        for rule in rules {
            validate_rules(&rule)?;
            self.instruments.insert(rule.symbol.clone(), rule);
        }
        Ok(())
    }

    pub fn install_balances(&mut self, balances: &[AssetBalance]) {
        self.assets.snapshot(balances);
    }

    /// Opening inventory baseline for strict Reducing.
    pub fn install_position(&mut self, instrument: impl Into<String>, quantity: f64) -> Result<(), RuntimeError> {
        if !quantity.is_finite() {
            return Err(RuntimeError::InvalidIntent("position must be finite".into()));
        }
        self.positions.insert(instrument.into(), quantity);
        Ok(())
    }

    pub fn install_policy(
        &mut self,
        limits: BTreeMap<String, risk::Limits>,
        state: risk::TradingState,
        instrument_states: BTreeMap<String, risk::TradingState>,
    ) {
        self.limits = limits;
        self.state = match state {
            risk::TradingState::Active => TradingState::Active,
            risk::TradingState::Reducing => TradingState::Reducing,
            risk::TradingState::Halted => TradingState::Halted,
        };
        self.instrument_states = instrument_states
            .into_iter()
            .map(|(symbol, state)| {
                let state = match state {
                    risk::TradingState::Active => TradingState::Active,
                    risk::TradingState::Reducing => TradingState::Reducing,
                    risk::TradingState::Halted => TradingState::Halted,
                };
                (symbol, state)
            })
            .collect();
    }

    pub fn set_allocation(&mut self, quote_asset: String, max_notional: f64, max_position_qty: Option<f64>) {
        self.allocation = Some((quote_asset, max_notional, max_position_qty));
    }

    pub fn set_max_price_age(&mut self, max_age: Duration) {
        self.max_price_age = Some(max_age);
    }

    pub fn cancel_requested(&self, client_order_id: &str) -> bool {
        self.orders
            .get(client_order_id)
            .is_some_and(|o| o.state == LocalState::CancelRequested)
    }

    pub fn working_client_ids(&self, instrument: &str) -> Vec<String> {
        self.orders
            .values()
            .filter(|o| o.instrument == instrument && o.state.is_working())
            .map(|o| o.client_order_id.clone())
            .collect()
    }

    /// Every order that may still rest at the venue, across instruments.
    pub fn all_working_client_ids(&self) -> Vec<String> {
        self.orders
            .values()
            .filter(|o| o.state.is_working())
            .map(|o| o.client_order_id.clone())
            .collect()
    }

    /// Undo a prepared order whose durable intent write failed.
    pub fn abort_order(&mut self, client_order_id: &str) {
        if let Some(order) = self.orders.remove(client_order_id) {
            self.assets.release(&order.reservation_asset, order.reserved_amount);
            if let Some(venue) = order.venue_order_id {
                self.by_venue.remove(&venue);
            }
        }
    }

    pub fn set_trading_state_active(&mut self) {
        self.state = TradingState::Active;
    }

    pub fn set_trading_state_reducing(&mut self) {
        self.state = TradingState::Reducing;
    }

    pub fn set_trading_state_halted(&mut self) {
        self.state = TradingState::Halted;
    }

    /// Conservative fee allowance reserved on buys, in basis points of notional.
    pub fn set_fee_allowance_bps(&mut self, bps: f64) -> Result<(), RuntimeError> {
        if !bps.is_finite() || bps < 0.0 {
            return Err(RuntimeError::InvalidConfig("fee allowance must be finite and non-negative".into()));
        }
        self.fee_allowance_bps = bps;
        Ok(())
    }

    pub fn identity(&self) -> &AccountIdentity {
        &self.identity
    }

    pub fn position(&self, instrument: &str) -> f64 {
        self.positions.get(instrument).copied().unwrap_or(0.0)
    }

    pub fn leaves(&self, client_order_id: &str) -> Option<f64> {
        self.orders.get(client_order_id).map(|o| o.leaves_quantity)
    }

    pub fn original_quantity(&self, client_order_id: &str) -> Option<f64> {
        self.orders.get(client_order_id).map(|o| o.original_quantity)
    }

    pub fn limit_price(&self, client_order_id: &str) -> Option<f64> {
        self.orders.get(client_order_id).and_then(|o| o.limit_price)
    }

    pub fn working_quantity(&self, instrument: &str, side: Side) -> f64 {
        self.orders
            .values()
            .filter(|o| o.instrument == instrument && o.side == side && o.state.is_working())
            .map(|o| o.leaves_quantity)
            .sum()
    }

    pub fn committed(&self, asset: &str) -> f64 {
        self.assets.committed(asset)
    }

    pub fn free(&self, asset: &str) -> f64 {
        self.assets.free(asset)
    }

    pub fn checkpoint(&self) -> BookCheckpoint {
        BookCheckpoint {
            positions: self.positions.clone(),
            asset_totals: self.assets.totals(),
            sequence: self.sequence,
            orders: self.orders.values().filter(|o| o.state.is_working()).cloned().collect(),
        }
    }

    /// Replaces positions, asset totals and open orders with the checkpoint's,
    /// charging each open order's reservation again.
    pub fn restore_checkpoint(&mut self, checkpoint: &BookCheckpoint) -> Result<(), RuntimeError> {
        if checkpoint.positions.values().any(|q| !q.is_finite()) {
            return Err(RuntimeError::CorruptState("checkpoint position is not finite".into()));
        }
        self.positions = checkpoint.positions.clone();
        self.assets = AssetLedger::default();
        self.assets.snapshot(
            &checkpoint
                .asset_totals
                .iter()
                .map(|(asset, total)| AssetBalance { asset: asset.clone(), free: *total, locked: 0.0 })
                .collect::<Vec<_>>(),
        );
        self.orders.clear();
        self.by_venue.clear();
        self.seen_trades.clear();
        for order in &checkpoint.orders {
            if !self.instruments.contains_key(&order.instrument) {
                return Err(RuntimeError::UnknownInstrument(order.instrument.clone()));
            }
            self.assets.reserve(&order.reservation_asset, order.reserved_amount)?;
            if let Some(venue) = &order.venue_order_id {
                self.by_venue.insert(venue.clone(), order.client_order_id.clone());
            }
            self.seen_trades.extend(order.trades.iter().cloned());
            self.orders.insert(order.client_order_id.clone(), order.clone());
        }
        self.sequence = self.sequence.max(checkpoint.sequence);
        Ok(())
    }

    /// Restore the last durable sequence after replay. Never moves backwards.
    pub fn restore_sequence(&mut self, sequence: u64) {
        self.sequence = self.sequence.max(sequence);
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Recreate a locally admitted order from the durable journal before
    /// replaying its venue events. The reservation is charged again from the
    /// restored opening inventory.
    pub fn restore_order(
        &mut self,
        submission: &Submission,
        intent: &Intent,
        reservation: &Reservation,
    ) -> Result<(), RuntimeError> {
        if self.orders.contains_key(&submission.client_order_id) {
            return Ok(());
        }
        if !self.instruments.contains_key(&intent.instrument) {
            return Err(RuntimeError::UnknownInstrument(intent.instrument.clone()));
        }
        self.assets.reserve(&reservation.asset, reservation.amount)?;
        self.sequence = self.sequence.max(submission.sequence);
        let per_unit = reservation.amount / intent.quantity;
        self.orders.insert(
            submission.client_order_id.clone(),
            RestingOrder {
                client_order_id: submission.client_order_id.clone(),
                venue_order_id: None,
                instrument: intent.instrument.clone(),
                side: intent.side,
                original_quantity: intent.quantity,
                executed_quantity: 0.0,
                leaves_quantity: intent.quantity,
                limit_price: intent.limit_price,
                state: LocalState::Pending,
                reservation_asset: reservation.asset.clone(),
                reserved_per_unit: per_unit,
                reserved_amount: reservation.amount,
                trades: Vec::new(),
            },
        );
        Ok(())
    }

    pub fn pending_client_ids(&self) -> Vec<String> {
        self.orders
            .values()
            .filter(|o| matches!(o.state, LocalState::Pending | LocalState::Unknown))
            .map(|o| o.client_order_id.clone())
            .collect()
    }

    pub fn order_symbol(&self, client_order_id: &str) -> Option<&str> {
        self.orders
            .get(client_order_id)
            .map(|o| o.instrument.as_str())
    }

    pub fn is_paused(&self) -> bool {
        self.pause_reason.is_some()
    }

    pub fn pause_reason(&self) -> Option<&str> {
        self.pause_reason.as_deref()
    }

    /// Explicit operator/recovery action. Callers must establish reconciliation
    /// first; this only clears the local admission gate.
    pub fn resume_after_recovery(&mut self) {
        self.pause_reason = None;
    }

    pub fn prepare(
        &mut self,
        intent: Intent,
        prices: &Prices,
        now: Instant,
    ) -> Result<PreparedOrder, RuntimeError> {
        if let Some(reason) = &self.pause_reason {
            return Err(RuntimeError::IncompleteRecovery(reason.clone()));
        }
        let rules = self
            .instruments
            .get(&intent.instrument)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownInstrument(intent.instrument.clone()))?;
        let limits = self.limits.get(&intent.instrument);
        let valuation = prices
            .get(&intent.instrument)
            .ok_or_else(|| RuntimeError::InvalidIntent("no prices for instrument".into()))?;
        if valuation.received_at > now {
            return Err(RuntimeError::CorruptState(
                "price receipt time is in the future".into(),
            ));
        }
        if let Some(max_age) = self.max_price_age {
            if now.saturating_duration_since(valuation.received_at) > max_age {
                return Err(RuntimeError::InvalidIntent(
                    "market data is older than the bootstrap freshness limit".into(),
                ));
            }
        }
        if !rules.operations.contains(&Operation::Place) {
            return Err(RuntimeError::InvalidIntent(
                "venue rules do not declare Place support".into(),
            ));
        }
        validate_intent(&intent)?;
        validate_precision(&intent, &rules)?;

        let quote_price = match intent.order_type {
            OrderType::Limit => intent.limit_price,
            OrderType::Market => {
                let spend = intent.max_quote_spend.ok_or_else(|| {
                    RuntimeError::InvalidIntent("market order requires a bounded spend".into())
                })?;
                let price = valuation.order.ok_or_else(|| {
                    RuntimeError::InvalidIntent("market order requires a fresh order price".into())
                })?;
                if !spend.is_finite() || spend <= 0.0 {
                    return Err(RuntimeError::InvalidIntent(
                        "market spend must be finite and positive".into(),
                    ));
                }
                if intent.quantity * price * rules.multiplier > spend {
                    return Err(RuntimeError::InvalidIntent(
                        "market quantity exceeds its bounded spend".into(),
                    ));
                }
                Some(price)
            }
        };

        validate_against_rules(&intent, &rules, quote_price)?;

        let exposure = self.exposure(&intent.instrument);
        let risk_limits = limits.copied().unwrap_or_default();
        let max_orders_per_sec = risk_limits.max_orders_per_sec;
        risk::evaluate(&risk::RiskInput {
            order: risk::OrderInput {
                side: risk_side(intent.side),
                quantity: intent.quantity,
                limit_price: if intent.order_type == OrderType::Limit { intent.limit_price } else { None },
                multiplier: rules.multiplier,
            },
            exposure,
            limits: risk_limits,
            state: risk_state(
                self.instrument_states
                    .get(&intent.instrument)
                    .copied()
                    .unwrap_or(self.state),
            ),
            valuation: risk::Valuation {
                order_price: if intent.order_type == OrderType::Market { valuation.order } else { None },
                position_price: valuation.position,
                reference_price: valuation.reference,
            },
        })
        .map_err(RuntimeError::Rejected)?;

        let (reservation_asset, reservation_amount, reserved_per_unit) =
            self.required_reservation(&intent, &rules, quote_price)?;
        if let Some((quote, max_notional, max_position)) = &self.allocation {
            if intent.side == Side::Buy
                && *quote == reservation_asset
                && self.assets.committed(quote) + reservation_amount > *max_notional
            {
                return Err(RuntimeError::Rejected(risk::Reject::new(
                    "allocation_notional",
                    "deployment allocation would be exceeded",
                )));
            }
            if let Some(max_position) = max_position {
                let projected = (exposure.position_qty
                    + match intent.side {
                        Side::Buy => exposure.working_buy_qty + intent.quantity,
                        Side::Sell => -exposure.working_sell_qty - intent.quantity,
                    })
                .abs();
                if projected > *max_position {
                    return Err(RuntimeError::Rejected(risk::Reject::new(
                        "allocation_position",
                        "deployment allocation position would be exceeded",
                    )));
                }
            }
        }
        if !self.assets.can_reserve(&reservation_asset, reservation_amount) {
            let available = self.assets.free(&reservation_asset);
            return Err(RuntimeError::InsufficientAssets {
                asset: reservation_asset,
                required: reservation_amount,
                available,
            });
        }
        if let Some(max) = max_orders_per_sec {
            if !self.rate_allow(max, now) {
                return Err(RuntimeError::RateLimited { limit: max });
            }
        }

        let sequence = self.sequence.saturating_add(1);
        let client_order_id = Uuid::new_v5(&self.identity.account_id, &sequence.to_be_bytes()).to_string();
        if self.orders.contains_key(&client_order_id) {
            return Err(RuntimeError::DuplicateOrder(client_order_id));
        }
        let reservation = self.assets.reserve(&reservation_asset, reservation_amount)?;

        self.sequence = sequence;
        if max_orders_per_sec.is_some() {
            self.rate.push_back(now);
        }
        let broker_request = BrokerOrderRequest {
            order_id: client_order_id.clone(),
            symbol: rules.venue_symbol.clone(),
            native_id: None,
            quantity: intent.quantity,
            side: match intent.side {
                Side::Buy => "buy".into(),
                Side::Sell => "sell".into(),
            },
            order_type: match intent.order_type {
                OrderType::Limit => "limit".into(),
                OrderType::Market => "market".into(),
            },
            time_in_force: match intent.time_in_force {
                TimeInForce::Gtc => "gtc".into(),
                TimeInForce::Ioc => "ioc".into(),
                TimeInForce::Fok => "fok".into(),
            },
            limit_price: intent.limit_price,
            external_account_ref: self.identity.external_account_ref.clone(),
        };
        let order = RestingOrder {
            client_order_id: client_order_id.clone(),
            venue_order_id: None,
            instrument: intent.instrument.clone(),
            side: intent.side,
            original_quantity: intent.quantity,
            executed_quantity: 0.0,
            leaves_quantity: intent.quantity,
            limit_price: intent.limit_price,
            state: LocalState::Pending,
            reservation_asset,
            reserved_per_unit,
            reserved_amount: reservation_amount,
            trades: Vec::new(),
        };
        self.orders.insert(client_order_id.clone(), order);

        Ok(PreparedOrder {
            submission: Submission { client_order_id, sequence },
            intent,
            broker_request,
            reservation,
            _sealed: (),
        })
    }

    pub fn apply(&mut self, event: &VenueEvent) -> Result<Vec<RuntimeEvent>, RuntimeError> {
        let account = self.identity.clone();
        match event {
            VenueEvent::Trade(trade) => {
                if !self.apply_trade(trade)? {
                    return Ok(Vec::new());
                }
                Ok(vec![RuntimeEvent::Venue { account, event: event.clone() }])
            }
            VenueEvent::Order(order) => {
                self.apply_order(order)?;
                Ok(vec![RuntimeEvent::Venue { account, event: event.clone() }])
            }
            VenueEvent::Balances { balances } => {
                self.assets.snapshot(balances);
                Ok(vec![RuntimeEvent::Venue { account, event: event.clone() }])
            }
            VenueEvent::Health { state, observed_at } => {
                self.health = match state {
                    StreamState::Live => BookHealth::Live,
                    StreamState::Stale => BookHealth::Stale,
                    StreamState::Closed => BookHealth::Closed,
                };
                Ok(vec![RuntimeEvent::Health {
                    account,
                    state: *state,
                    observed_at: *observed_at,
                }])
            }
        }
    }

    pub fn request_cancel(&mut self, id: &str) -> Result<CancelRequest, RuntimeError> {
        let order = self
            .orders
            .get_mut(id)
            .ok_or_else(|| RuntimeError::UnknownOrder(id.to_string()))?;
        if !order.state.is_working() {
            return Err(RuntimeError::OrderNotWorking(id.to_string()));
        }
        order.state = LocalState::CancelRequested;
        Ok(CancelRequest {
            client_order_id: order.client_order_id.clone(),
            venue_order_id: order.venue_order_id.clone(),
            symbol: order.instrument.clone(),
        })
    }

    fn exposure(&self, instrument: &str) -> risk::Exposure {
        risk::Exposure {
            position_qty: self.position(instrument),
            working_buy_qty: self.working_quantity(instrument, Side::Buy),
            working_sell_qty: self.working_quantity(instrument, Side::Sell),
        }
    }

    fn required_reservation(
        &self,
        intent: &Intent,
        rules: &InstrumentRules,
        quote_price: Option<f64>,
    ) -> Result<(String, f64, f64), RuntimeError> {
        match intent.side {
            Side::Buy => {
                let price = quote_price.ok_or_else(|| {
                    RuntimeError::InvalidIntent("buy requires a price to reserve quote currency".into())
                })?;
                let notional = if intent.order_type == OrderType::Market {
                    intent.max_quote_spend.unwrap_or(intent.quantity * price * rules.multiplier)
                } else {
                    intent.quantity * price * rules.multiplier
                };
                let fee = notional * self.fee_allowance_bps / 10_000.0;
                let amount = notional + fee;
                Ok((rules.quote_asset.clone(), amount, amount / intent.quantity))
            }
            Side::Sell => {
                let per_unit = rules.multiplier;
                let amount = intent.quantity * per_unit;
                Ok((rules.base_asset.clone(), amount, per_unit))
            }
        }
    }

    fn rate_allow(&mut self, max: u32, now: Instant) -> bool {
        prune(&mut self.rate, now);
        self.rate.len() < max as usize
    }

    fn apply_trade(&mut self, trade: &Trade) -> Result<bool, RuntimeError> {
        if trade.key.venue != self.identity.venue
            || trade.key.account_id != self.identity.account_id
            || trade.key.environment != self.identity.environment
        {
            return Err(RuntimeError::CorruptState(
                "fill belongs to another venue/account/environment".into(),
            ));
        }
        if self.seen_trades.contains(&trade.key) {
            return Ok(false);
        }
        let client = if !trade.client_order_id.is_empty() {
            self.orders.get(&trade.client_order_id).map(|_| trade.client_order_id.clone())
        } else {
            self.by_venue.get(&trade.venue_order_id).cloned()
        };
        let Some(client) = client else {
            self.pause_reason = Some(format!(
                "fill for unknown order {} on {}",
                trade.venue_order_id, trade.key.symbol
            ));
            return Err(RuntimeError::IncompleteRecovery(
                self.pause_reason.clone().unwrap_or_default(),
            ));
        };
        let order = self
            .orders
            .get_mut(&client)
            .ok_or_else(|| RuntimeError::CorruptState(format!("order {client} vanished")))?;
        if order.instrument != trade.key.symbol || order.side != trade.side {
            return Err(RuntimeError::CorruptState(format!(
                "fill {} does not match order {client}",
                trade.key.trade_id
            )));
        }

        self.seen_trades.insert(trade.key.clone());
        order.trades.push(trade.key.clone());
        order.executed_quantity += trade.quantity;
        order.leaves_quantity = (order.leaves_quantity - trade.quantity).max(0.0);
        let signed = match trade.side {
            Side::Buy => trade.quantity,
            Side::Sell => -trade.quantity,
        };
        *self.positions.entry(order.instrument.clone()).or_insert(0.0) += signed;
        if !trade.venue_order_id.is_empty() {
            order.venue_order_id = Some(trade.venue_order_id.clone());
            self.by_venue.insert(trade.venue_order_id.clone(), client.clone());
        }

        let release = if order.leaves_quantity <= 0.0 {
            order.reserved_amount
        } else {
            (trade.quantity * order.reserved_per_unit).min(order.reserved_amount)
        };
        order.reserved_amount = (order.reserved_amount - release).max(0.0);
        self.assets.release(&order.reservation_asset, release);

        order.state = if order.leaves_quantity <= 0.0 {
            LocalState::Filled
        } else if order.state == LocalState::CancelRequested {
            LocalState::CancelRequested
        } else {
            LocalState::PartiallyFilled
        };
        Ok(true)
    }

    fn apply_order(&mut self, event: &RuntimeOrder) -> Result<(), RuntimeError> {
        let client = if !event.client_order_id.is_empty() {
            self.orders.get(&event.client_order_id).map(|_| event.client_order_id.clone())
        } else {
            self.by_venue.get(&event.venue_order_id).cloned()
        };
        let Some(client) = client else {
            self.pause_reason = Some(format!(
                "unknown external order {} on {}",
                event.venue_order_id, event.symbol
            ));
            return Err(RuntimeError::IncompleteRecovery(
                self.pause_reason.clone().unwrap_or_default(),
            ));
        };
        let order = self
            .orders
            .get_mut(&client)
            .ok_or_else(|| RuntimeError::CorruptState(format!("order {client} vanished")))?;
        if order.instrument != event.symbol {
            return Err(RuntimeError::CorruptState(format!(
                "order report for {} does not match order {client}",
                event.symbol
            )));
        }
        order.executed_quantity = order.executed_quantity.max(event.executed_quantity);
        if !event.venue_order_id.is_empty() {
            order.venue_order_id = Some(event.venue_order_id.clone());
            self.by_venue.insert(event.venue_order_id.clone(), client);
        }
        let terminal = matches!(
            event.state,
            RuntimeOrderState::Filled
                | RuntimeOrderState::Canceled
                | RuntimeOrderState::Rejected
                | RuntimeOrderState::Expired
        );
        if terminal {
            order.leaves_quantity = 0.0;
            order.state = match event.state {
                RuntimeOrderState::Filled => LocalState::Filled,
                RuntimeOrderState::Canceled => LocalState::Canceled,
                RuntimeOrderState::Rejected => LocalState::Rejected,
                RuntimeOrderState::Expired => LocalState::Expired,
                _ => order.state,
            };
            let release = order.reserved_amount;
            order.reserved_amount = 0.0;
            self.assets.release(&order.reservation_asset, release);
        } else {
            order.leaves_quantity = order.leaves_quantity.min(event.leaves_quantity);
            if order.state != LocalState::CancelRequested {
                order.state = match event.state {
                    RuntimeOrderState::PartiallyFilled => LocalState::PartiallyFilled,
                    RuntimeOrderState::New => LocalState::Live,
                    RuntimeOrderState::Unknown => LocalState::Unknown,
                    _ => order.state,
                };
            }
        }
        Ok(())
    }
}

fn prune(window: &mut VecDeque<Instant>, now: Instant) {
    let cutoff = now.checked_sub(Duration::from_secs(1)).unwrap_or(now);
    while window.front().is_some_and(|t| *t <= cutoff) {
        window.pop_front();
    }
}

fn risk_side(side: Side) -> risk::Side {
    match side {
        Side::Buy => risk::Side::Buy,
        Side::Sell => risk::Side::Sell,
    }
}

fn risk_state(state: TradingState) -> risk::TradingState {
    match state {
        TradingState::Active => risk::TradingState::Active,
        TradingState::Reducing => risk::TradingState::Reducing,
        TradingState::Halted => risk::TradingState::Halted,
    }
}

fn validate_limits(limits: &risk::Limits) -> Result<(), String> {
    let bad = [
        limits.max_order_qty,
        limits.max_order_notional,
        limits.max_position_qty,
        limits.max_position_notional,
        limits.price_band_pct,
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.is_finite() || value < 0.0);
    if bad || limits.max_orders_per_sec == Some(0) {
        return Err("limits must be finite, non-negative and rate must be positive".into());
    }
    Ok(())
}

fn validate_rules(rules: &InstrumentRules) -> Result<(), RuntimeError> {
    if rules.symbol.trim().is_empty()
        || rules.venue_symbol.trim().is_empty()
        || rules.base_asset.trim().is_empty()
        || rules.quote_asset.trim().is_empty()
    {
        return Err(RuntimeError::InvalidConfig("instrument identity is incomplete".into()));
    }
    if !rules.tick_size.is_finite()
        || rules.tick_size <= 0.0
        || !rules.step_size.is_finite()
        || rules.step_size <= 0.0
        || !rules.multiplier.is_finite()
        || rules.multiplier <= 0.0
    {
        return Err(RuntimeError::InvalidConfig(
            "tick, step and multiplier must be finite and positive".into(),
        ));
    }
    Ok(())
}

fn validate_intent(intent: &Intent) -> Result<(), RuntimeError> {
    if !intent.quantity.is_finite() || intent.quantity <= 0.0 {
        return Err(RuntimeError::InvalidIntent("quantity must be finite and positive".into()));
    }
    match intent.order_type {
        OrderType::Limit => {
            let price = intent.limit_price.ok_or_else(|| {
                RuntimeError::InvalidIntent("limit order requires a limit price".into())
            })?;
            if !price.is_finite() || price <= 0.0 {
                return Err(RuntimeError::InvalidIntent("limit price must be finite and positive".into()));
            }
        }
        OrderType::Market => {
            if intent.limit_price.is_some() {
                return Err(RuntimeError::InvalidIntent("market order cannot carry a limit price".into()));
            }
            if intent.max_quote_spend.is_none() {
                return Err(RuntimeError::InvalidIntent(
                    "market order requires a bounded quote spend".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_precision(intent: &Intent, rules: &InstrumentRules) -> Result<(), RuntimeError> {
    if !aligned(intent.quantity, rules.step_size) {
        return Err(RuntimeError::InvalidIntent(format!(
            "quantity {} is not a multiple of step {}",
            intent.quantity, rules.step_size
        )));
    }
    if let Some(price) = intent.limit_price {
        if !aligned(price, rules.tick_size) {
            return Err(RuntimeError::InvalidIntent(format!(
                "price {} is not a multiple of tick {}",
                price, rules.tick_size
            )));
        }
    }
    Ok(())
}

fn validate_against_rules(
    intent: &Intent,
    rules: &InstrumentRules,
    price: Option<f64>,
) -> Result<(), RuntimeError> {
    if let Some(min) = rules.min_quantity {
        if intent.quantity < min {
            return Err(RuntimeError::InvalidIntent(format!(
                "quantity {} is below minimum {min}",
                intent.quantity
            )));
        }
    }
    if let Some(max) = rules.max_quantity {
        if intent.quantity > max {
            return Err(RuntimeError::InvalidIntent(format!(
                "quantity {} is above maximum {max}",
                intent.quantity
            )));
        }
    }
    if let Some(price) = price {
        let notional = intent.quantity * price * rules.multiplier;
        if let Some(min) = rules.min_notional {
            if notional < min {
                return Err(RuntimeError::InvalidIntent(format!(
                    "notional {notional} is below minimum {min}"
                )));
            }
        }
        if let Some(max) = rules.max_notional {
            if notional > max {
                return Err(RuntimeError::InvalidIntent(format!(
                    "notional {notional} is above maximum {max}"
                )));
            }
        }
    }
    Ok(())
}

/// Precision is never rounded down: an unaligned risk-increasing value is
/// rejected, not silently shrunk.
fn aligned(value: f64, increment: f64) -> bool {
    if !value.is_finite() || increment <= 0.0 {
        return false;
    }
    let steps = value / increment;
    (steps - steps.round()).abs() <= 1e-9 * steps.abs().max(1.0)
}
