use crate::adapter::VenueAdapter;
use async_trait::async_trait;
use parallax_types::{
    AckStatus, CanonicalContractId, ClientOrderId, ExecError, FeeModel, OrderAck, OrderId,
    OrderIntent, OrderType, Position, SettlementModel, Side, Timestamp, VenueCapabilities, VenueId,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy)]
struct MarketState {
    bid: f64,
    bid_size: f64,
    ask: f64,
    ask_size: f64,
}

/// A resting or in-flight order plus the strictly-increasing sequence
/// number it was submitted with — the tie-breaker for price-time
/// priority when two orders share a price (design doc review 2.3).
#[derive(Clone)]
struct Tracked {
    intent: OrderIntent,
    sequence: u64,
}

struct PaperState {
    resting: BTreeMap<OrderId, Tracked>,
    /// Orders submitted but not yet "arrived" on the simulated book —
    /// only populated when `PaperConfig::latency_ns` is nonzero. Moved
    /// into `resting` (or resolved immediately, for an IOC) the first
    /// time `advance_market` reports a tick at or after the order's
    /// arrival time (design doc review 4.6).
    pending: BTreeMap<OrderId, (Tracked, i64)>,
    market: BTreeMap<CanonicalContractId, MarketState>,
    /// Every ack this venue has ever produced, keyed by the deterministic
    /// id `ClientOrderId::derive` computes from the order that produced
    /// it — overwritten with an order's latest status as it moves through
    /// Accepted -> PartiallyFilled -> Filled, so a lookup always returns
    /// the most recent known state. What `find_order_by_client_id`
    /// answers from (docs/GOING-LIVE.md Stage 1's idempotent-retry rule:
    /// "before any retry, query order state by client ID"). A `HashMap`,
    /// not a `BTreeMap` like the maps above — this one is never iterated
    /// in a way whose order matters, only looked up by key, so there's no
    /// determinism requirement to buy a `BTreeMap`'s ordering for.
    by_client_id: HashMap<ClientOrderId, OrderAck>,
}

/// Models the two most flattering assumptions a paper venue can make
/// about a market maker, and lets a caller turn either off (design doc
/// review 3.26).
#[derive(Debug, Clone, Copy)]
pub struct PaperConfig {
    /// Fraction of incoming crossing liquidity assumed to already be
    /// ahead of our resting order in the venue's real queue at that price
    /// level. `0.0` — always at the front — is the default and the most
    /// optimistic value available; it overstates fill probability for a
    /// market maker, whose whole edge depends on realistic queue
    /// position.
    pub queue_ahead_fraction: f64,
    /// Simulated one-way latency in nanoseconds. An order does not become
    /// live on the simulated book until `submitted_at + latency_ns`; a
    /// zero-latency backtest (the default) lets a strategy fill against
    /// the exact quote it just reacted to, which no real network round
    /// trip permits.
    pub latency_ns: i64,
    /// The fee schedule `capabilities()` reports, and every fill in this
    /// venue's backtests is charged against. `FeeModel::default()` — zero
    /// maker and taker rates — is the default here for the same reason
    /// `queue_ahead_fraction`/`latency_ns` default to their most
    /// flattering values: it keeps `PaperAdapter::new()` a deterministic,
    /// cost-free matching-engine primitive for unit tests that assert
    /// exact fill prices/quantities. A caller measuring whether a
    /// strategy actually has edge must override this explicitly with
    /// `FeeModel::kalshi_default()`/`polymarket_default()` — a zero fee
    /// model silently reports an idealization, not a result.
    pub fee_model: FeeModel,
}

impl Default for PaperConfig {
    fn default() -> Self {
        PaperConfig {
            queue_ahead_fraction: 0.0,
            latency_ns: 0,
            fee_model: FeeModel::default(),
        }
    }
}

/// The in-memory simulated venue (design doc §4: "not a stub to be
/// deleted later, it is a first-class adapter target"). Implements real
/// limit-order-book matching semantics — a marketable limit crosses
/// immediately for whatever size is available and rests the remainder;
/// an IOC order fills whatever it can immediately and cancels the rest;
/// a passive resting limit fills later when `advance_market` reports a
/// tick that crosses it, at *its own* price, not the crossing tick's
/// price (design doc review 2.2). This is what `parallax-sim` drives
/// during replay/backtest and what shadow mode drives during live
/// bake-in.
pub struct PaperAdapter {
    state: Mutex<PaperState>,
    next_id: AtomicU64,
    next_sequence: AtomicU64,
    config: PaperConfig,
}

impl PaperAdapter {
    pub fn new() -> Self {
        Self::with_config(PaperConfig::default())
    }

    pub fn with_config(config: PaperConfig) -> Self {
        PaperAdapter {
            state: Mutex::new(PaperState {
                resting: BTreeMap::new(),
                pending: BTreeMap::new(),
                market: BTreeMap::new(),
                by_client_id: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
            next_sequence: AtomicU64::new(1),
            config,
        }
    }

    fn next_order_id(&self) -> OrderId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        OrderId(format!("paper-{n}"))
    }

    fn next_sequence(&self) -> u64 {
        self.next_sequence.fetch_add(1, Ordering::Relaxed)
    }

    fn try_immediate_match(market: Option<&MarketState>, intent: &OrderIntent) -> (f64, f64) {
        match (intent.side, market) {
            (Side::Buy, Some(m)) if intent.price >= m.ask => (intent.size.min(m.ask_size), m.ask),
            (Side::Sell, Some(m)) if intent.price <= m.bid => (intent.size.min(m.bid_size), m.bid),
            _ => (0.0, 0.0),
        }
    }

    /// `rest_remainder` is `true` for a `Limit` order (any unfilled size
    /// rests) and `false` for an IOC (any unfilled size is canceled,
    /// never rested) — this is what governs the zero-fill case too: a
    /// `Limit` order that hasn't crossed yet is `Accepted` and rested in
    /// full, not rejected, which only applies to an IOC that found
    /// nothing to take.
    fn ack_for_immediate_result(
        order_id: OrderId,
        intent: &OrderIntent,
        filled_qty: f64,
        fill_price: f64,
        rest_remainder: bool,
    ) -> (OrderAck, Option<Tracked>, Option<f64>) {
        let remaining = intent.size - filled_qty;
        let ts = intent.created_at;

        if filled_qty <= 0.0 {
            return if rest_remainder {
                (
                    OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status: AckStatus::Accepted,
                        ts,
                    },
                    Some(Tracked {
                        intent: intent.clone(),
                        sequence: 0, // caller overwrites with a real sequence before storing
                    }),
                    Some(intent.size),
                )
            } else {
                (
                    OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status: AckStatus::Rejected {
                            reason: "no crossing liquidity available".into(),
                        },
                        ts,
                    },
                    None,
                    None,
                )
            };
        }

        if remaining > 1e-9 {
            let status = AckStatus::PartiallyFilled {
                filled_qty,
                remaining_qty: remaining,
                price: fill_price,
            };
            if rest_remainder {
                (
                    OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status,
                        ts,
                    },
                    Some(Tracked {
                        intent: intent.clone(),
                        sequence: 0,
                    }),
                    Some(remaining),
                )
            } else {
                // IOC: whatever didn't fill is canceled, not rested.
                (
                    OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status,
                        ts,
                    },
                    None,
                    None,
                )
            }
        } else {
            (
                OrderAck {
                    order_id,
                    venue: VenueId::Paper,
                    status: AckStatus::Filled {
                        qty: filled_qty,
                        price: fill_price,
                    },
                    ts,
                },
                None,
                None,
            )
        }
    }

    /// Activates every pending order for `contract` whose arrival time
    /// has passed, evaluating each against `market` (the tick that just
    /// arrived) exactly once: an IOC fills-or-cancels immediately, a
    /// limit that crosses fills/partially fills, and a limit that doesn't
    /// cross starts resting from this point on. Never activates an order
    /// against a market snapshot from before it was actually live —
    /// that's the zero-latency snipe this whole mechanism exists to rule
    /// out.
    fn activate_pending(
        state: &mut PaperState,
        contract: &CanonicalContractId,
        market: MarketState,
        now: Timestamp,
    ) -> Vec<OrderAck> {
        let ready_ids: Vec<OrderId> = state
            .pending
            .iter()
            .filter(|(_, (t, arrival))| {
                &t.intent.contract == contract && *arrival <= now.as_nanos()
            })
            .map(|(id, _)| id.clone())
            .collect();

        let mut acks = Vec::new();
        for id in ready_ids {
            // `ready_ids` was just collected from `state.pending` above,
            // and nothing between that collection and this removal can
            // mutate `state.pending` — the whole `advance_market` call
            // holds `self.state`'s lock for its entire duration, so no
            // concurrent submit()/cancel() can race this loop.
            let (tracked, _arrival) = state
                .pending
                .remove(&id)
                .expect("id was just read from state.pending under the same lock");
            let (filled_qty, fill_price) =
                Self::try_immediate_match(Some(&market), &tracked.intent);
            let rest_remainder = matches!(tracked.intent.order_type, OrderType::Limit);
            let (ack, to_rest, remaining) = Self::ack_for_immediate_result(
                id.clone(),
                &tracked.intent,
                filled_qty,
                fill_price,
                rest_remainder,
            );
            if let (Some(mut t), Some(remaining_qty)) = (to_rest, remaining) {
                t.sequence = tracked.sequence;
                t.intent.size = remaining_qty;
                state.resting.insert(id, t);
            }
            state
                .by_client_id
                .insert(ClientOrderId::derive(&tracked.intent), ack.clone());
            acks.push(ack);
        }
        acks
    }

    /// Feed the venue's current best bid/ask for a contract — called by
    /// the sim harness on each replayed tick, or by a live market-data
    /// task in shadow mode. Returns every ack this update triggers: fills
    /// against previously-resting orders, activations/resolutions of
    /// orders whose simulated latency has just elapsed, at most one per
    /// order per call.
    ///
    /// Resting orders competing for the same side are matched in
    /// price-time priority — sorted by price advantage, then by
    /// submission sequence — against a running remaining-liquidity
    /// counter, itself first haircut by `PaperConfig::queue_ahead_fraction`
    /// to model liquidity already claimed by other participants ahead of
    /// us in the real queue. `BTreeMap`, not `HashMap`: iteration order
    /// must not depend on the process's hash seed, or the same input
    /// produces a different fill allocation — and therefore a different
    /// P&L — between runs (design doc review 2.3).
    pub fn advance_market(
        &self,
        contract: CanonicalContractId,
        bid: f64,
        bid_size: f64,
        ask: f64,
        ask_size: f64,
        now: Timestamp,
    ) -> Vec<OrderAck> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let market = MarketState {
            bid,
            bid_size,
            ask,
            ask_size,
        };

        let mut acks = Self::activate_pending(&mut state, &contract, market, now);

        let mut candidates: Vec<(OrderId, Tracked)> = state
            .resting
            .iter()
            .filter(|(_, t)| t.intent.contract == contract)
            .map(|(id, t)| (id.clone(), t.clone()))
            .collect();
        candidates.sort_by(|(_, a), (_, b)| match a.intent.side {
            Side::Buy => b
                .intent
                .price
                .total_cmp(&a.intent.price)
                .then(a.sequence.cmp(&b.sequence)),
            Side::Sell => a
                .intent
                .price
                .total_cmp(&b.intent.price)
                .then(a.sequence.cmp(&b.sequence)),
        });

        let queue_haircut = 1.0 - self.config.queue_ahead_fraction.clamp(0.0, 1.0);
        let mut remaining_ask_size = ask_size * queue_haircut;
        let mut remaining_bid_size = bid_size * queue_haircut;

        for (id, tracked) in candidates {
            let intent = &tracked.intent;
            let crosses = match intent.side {
                Side::Buy => ask <= intent.price,
                Side::Sell => bid >= intent.price,
            };
            if !crosses {
                continue;
            }
            let avail = match intent.side {
                Side::Buy => &mut remaining_ask_size,
                Side::Sell => &mut remaining_bid_size,
            };
            let filled_qty = intent.size.min(*avail);
            if filled_qty <= 0.0 {
                continue;
            }
            *avail -= filled_qty;

            // A resting order fills at *its own* limit price when hit —
            // not the aggressor's price. Filling a resting bid at the
            // incoming ask handed every passive fill free price
            // improvement in every backtest (design doc review 2.2).
            let fill_price = intent.price;

            if filled_qty + 1e-9 < intent.size {
                let mut remainder = tracked.clone();
                remainder.intent.size -= filled_qty;
                state.resting.insert(id.clone(), remainder);
            } else {
                state.resting.remove(&id);
            }
            let ack = OrderAck {
                order_id: id,
                venue: VenueId::Paper,
                status: AckStatus::Filled {
                    qty: filled_qty,
                    price: fill_price,
                },
                ts: now,
            };
            state
                .by_client_id
                .insert(ClientOrderId::derive(intent), ack.clone());
            acks.push(ack);
        }

        state.market.insert(contract, market);
        acks
    }
}

impl Default for PaperAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VenueAdapter for PaperAdapter {
    fn venue_id(&self) -> VenueId {
        VenueId::Paper
    }

    fn capabilities(&self) -> VenueCapabilities {
        VenueCapabilities {
            venue: VenueId::Paper,
            settlement: SettlementModel::Simulated,
            min_tick: 0.01,
            min_order_size: 1.0,
            fee_model: self.config.fee_model,
            rate_limit_per_sec: u32::MAX,
        }
    }

    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError> {
        order.validate().map_err(|e| ExecError::Rejected {
            venue: VenueId::Paper,
            reason: e.to_string(),
        })?;

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let order_id = self.next_order_id();
        let sequence = self.next_sequence();

        if self.config.latency_ns > 0 {
            // Not live yet: goes on the wire, resolved the moment
            // `advance_market` reports a tick at or after arrival.
            let arrival = order
                .created_at
                .as_nanos()
                .saturating_add(self.config.latency_ns);
            state.pending.insert(
                order_id.clone(),
                (
                    Tracked {
                        intent: order.clone(),
                        sequence,
                    },
                    arrival,
                ),
            );
            let ack = OrderAck {
                order_id,
                venue: VenueId::Paper,
                status: AckStatus::Accepted,
                ts: order.created_at,
            };
            state
                .by_client_id
                .insert(ClientOrderId::derive(&order), ack.clone());
            return Ok(ack);
        }

        let market = state.market.get(&order.contract).copied();
        let (filled_qty, fill_price) = Self::try_immediate_match(market.as_ref(), &order);

        // Deplete the resting liquidity this fill just consumed so a
        // second order submitted before the next `advance_market` call
        // — e.g. two strategy engines both reacting to the same tick —
        // sees what's actually left rather than the original quote size
        // twice over.
        if filled_qty > 0.0 {
            if let Some(m) = state.market.get_mut(&order.contract) {
                match order.side {
                    Side::Buy => m.ask_size = (m.ask_size - filled_qty).max(0.0),
                    Side::Sell => m.bid_size = (m.bid_size - filled_qty).max(0.0),
                }
            }
        }

        let ack = match order.order_type {
            OrderType::ImmediateOrCancel => {
                let (ack, _, _) =
                    Self::ack_for_immediate_result(order_id, &order, filled_qty, fill_price, false);
                ack
            }
            OrderType::Limit => {
                let (ack, to_rest, remaining) = Self::ack_for_immediate_result(
                    order_id.clone(),
                    &order,
                    filled_qty,
                    fill_price,
                    true,
                );
                if let (Some(mut t), Some(remaining_qty)) = (to_rest, remaining) {
                    t.sequence = sequence;
                    t.intent.size = remaining_qty;
                    state.resting.insert(order_id, t);
                }
                ack
            }
        };
        state
            .by_client_id
            .insert(ClientOrderId::derive(&order), ack.clone());
        Ok(ack)
    }

    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.resting.remove(&order_id).is_some() || state.pending.remove(&order_id).is_some() {
            Ok(())
        } else {
            Err(ExecError::NotFound(order_id))
        }
    }

    async fn find_order_by_client_id(
        &self,
        client_order_id: &ClientOrderId,
    ) -> Result<Option<OrderAck>, ExecError> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Ok(state.by_client_id.get(client_order_id).cloned())
    }

    /// A paper matching engine isn't an account with a ledger, and
    /// intentionally doesn't keep one — `RiskGate`'s own position map,
    /// built from every ack this adapter has ever produced, is already
    /// the single source of truth for a backtest's positions. Returning
    /// an empty list here (rather than reconstructing a second, parallel
    /// position ledger that could drift from the first) is the honest
    /// answer, not an unimplemented stub.
    async fn fetch_positions(&self) -> Result<Vec<Position>, ExecError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::{CanonicalContractSpec, Direction, EngineId, EventClass, Outcome};

    fn contract() -> CanonicalContractId {
        CanonicalContractSpec {
            event_class: EventClass("wx.temp".into()),
            location: "chicago".into(),
            threshold: 869,
            direction: Direction::GreaterThan,
            resolution_window: "2026-08-12".into(),
            resolution_source: "nws_official".into(),
        }
        .to_id()
    }

    fn order(side: Side, price: f64, size: f64, order_type: OrderType) -> OrderIntent {
        OrderIntent {
            venue: VenueId::Paper,
            contract: contract(),
            outcome: Outcome::Yes,
            side,
            price,
            size,
            order_type,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(0),
        }
    }

    #[tokio::test]
    async fn ioc_fills_immediately_when_crossing_available_liquidity() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );

        let ack = venue
            .submit(order(Side::Buy, 0.65, 20.0, OrderType::ImmediateOrCancel))
            .await
            .unwrap();
        match ack.status {
            AckStatus::Filled { qty, price } => {
                assert_eq!(qty, 20.0);
                assert_eq!(price, 0.63);
            }
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_orders_in_the_same_tick_cannot_both_fill_against_the_same_liquidity() {
        // Regression test: a venue update reports 50 units available at
        // the ask. Two separate strategy engines each submit a 40-unit
        // marketable order reacting to that same tick. Together they
        // want 80 units, but only 50 actually exist — the second order
        // must not report a full fill against the same nominal size the
        // first one already consumed.
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            50.0,
            Timestamp::from_nanos(0),
        );

        let first = venue
            .submit(order(Side::Buy, 0.63, 40.0, OrderType::ImmediateOrCancel))
            .await
            .unwrap();
        let first_qty = match first.status {
            AckStatus::Filled { qty, .. } => qty,
            other => panic!("expected first order to fill, got {other:?}"),
        };
        assert_eq!(first_qty, 40.0);

        let second = venue
            .submit(order(Side::Buy, 0.63, 40.0, OrderType::ImmediateOrCancel))
            .await
            .unwrap();
        let second_qty = match second.status {
            AckStatus::Filled { qty, .. } => qty,
            AckStatus::PartiallyFilled { filled_qty, .. } => filled_qty,
            AckStatus::Rejected { .. } => 0.0,
            other => panic!("unexpected status {other:?}"),
        };

        assert!(
            first_qty + second_qty <= 50.0 + 1e-9,
            "combined fills {first_qty} + {second_qty} exceeded the venue's quoted 50 units of liquidity"
        );
        assert_eq!(
            second_qty, 10.0,
            "only 10 units should have been left after the first order took 40"
        );
    }

    #[tokio::test]
    async fn ioc_is_rejected_when_it_does_not_cross() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );
        let ack = venue
            .submit(order(Side::Buy, 0.61, 20.0, OrderType::ImmediateOrCancel))
            .await
            .unwrap();
        assert!(matches!(ack.status, AckStatus::Rejected { .. }));
    }

    #[tokio::test]
    async fn passive_limit_rests_until_market_crosses_it() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );

        let ack = venue
            .submit(order(Side::Buy, 0.58, 15.0, OrderType::Limit))
            .await
            .unwrap();
        assert_eq!(ack.status, AckStatus::Accepted);

        // Market hasn't moved far enough yet.
        let fills = venue.advance_market(
            contract(),
            0.55,
            100.0,
            0.60,
            100.0,
            Timestamp::from_nanos(1),
        );
        assert!(fills.is_empty());

        // Now the ask drops to meet our resting bid.
        let fills = venue.advance_market(
            contract(),
            0.50,
            100.0,
            0.58,
            100.0,
            Timestamp::from_nanos(2),
        );
        assert_eq!(fills.len(), 1);
        match &fills[0].status {
            AckStatus::Filled { qty, price } => {
                assert_eq!(*qty, 15.0);
                assert_eq!(*price, 0.58);
            }
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_resting_order_fills_at_its_own_price_not_the_crossing_ticks_price() {
        // Regression for design doc review 2.2: a resting bid at 0.58,
        // hit by a tick whose ask has dropped all the way to 0.40, must
        // still fill at 0.58 (its own price) — not 0.40, which would be
        // free price improvement no real resting order receives.
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );
        venue
            .submit(order(Side::Buy, 0.58, 15.0, OrderType::Limit))
            .await
            .unwrap();

        let fills = venue.advance_market(
            contract(),
            0.35,
            100.0,
            0.40,
            100.0,
            Timestamp::from_nanos(1),
        );
        assert_eq!(fills.len(), 1);
        match &fills[0].status {
            AckStatus::Filled { price, .. } => assert_eq!(*price, 0.58),
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_removes_a_resting_order() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );
        let ack = venue
            .submit(order(Side::Buy, 0.58, 15.0, OrderType::Limit))
            .await
            .unwrap();

        venue.cancel(ack.order_id.clone()).await.unwrap();

        // Market crossing the old resting price should no longer produce a fill.
        let fills = venue.advance_market(
            contract(),
            0.50,
            100.0,
            0.50,
            100.0,
            Timestamp::from_nanos(1),
        );
        assert!(fills.is_empty());

        assert!(matches!(
            venue.cancel(ack.order_id).await,
            Err(ExecError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn marketable_limit_fills_immediately_and_rests_any_remainder() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            10.0,
            Timestamp::from_nanos(0),
        );

        // Buy limit at 0.65 crosses the 0.63 ask, but only 10 units are available there.
        let ack = venue
            .submit(order(Side::Buy, 0.65, 30.0, OrderType::Limit))
            .await
            .unwrap();
        match ack.status {
            AckStatus::PartiallyFilled {
                filled_qty,
                remaining_qty,
                price,
            } => {
                assert_eq!(filled_qty, 10.0);
                assert_eq!(remaining_qty, 20.0);
                assert_eq!(price, 0.63);
            }
            other => panic!("expected PartiallyFilled, got {other:?}"),
        }

        // The remaining 20 should now be resting and fillable on a later cross.
        let fills = venue.advance_market(
            contract(),
            0.55,
            100.0,
            0.60,
            100.0,
            Timestamp::from_nanos(1),
        );
        assert_eq!(fills.len(), 1);
        assert!(matches!(fills[0].status, AckStatus::Filled { qty, .. } if qty == 20.0));
    }

    #[tokio::test]
    async fn two_resting_buys_are_filled_in_price_priority_not_arrival_order() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.70,
            100.0,
            Timestamp::from_nanos(0),
        );
        // Worse price submitted first.
        let worse = venue
            .submit(order(Side::Buy, 0.61, 10.0, OrderType::Limit))
            .await
            .unwrap();
        let better = venue
            .submit(order(Side::Buy, 0.65, 10.0, OrderType::Limit))
            .await
            .unwrap();
        assert_eq!(worse.status, AckStatus::Accepted);
        assert_eq!(better.status, AckStatus::Accepted);

        // Only 10 units of crossing liquidity arrive — the better (higher)
        // price should win it regardless of submission order.
        let fills = venue.advance_market(
            contract(),
            0.50,
            100.0,
            0.60,
            10.0,
            Timestamp::from_nanos(1),
        );
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].order_id, better.order_id);
    }

    #[tokio::test]
    async fn latency_prevents_sniping_a_quote_that_was_already_gone_on_arrival() {
        let venue = PaperAdapter::with_config(PaperConfig {
            queue_ahead_fraction: 0.0,
            latency_ns: 1_000_000_000, // 1s
            ..PaperConfig::default()
        });
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            50.0,
            Timestamp::from_nanos(0),
        );

        let mut ioc = order(Side::Buy, 0.65, 20.0, OrderType::ImmediateOrCancel);
        ioc.created_at = Timestamp::from_nanos(0);
        let ack = venue.submit(ioc).await.unwrap();
        // Not resolved yet: the order is still in flight.
        assert_eq!(ack.status, AckStatus::Accepted);

        // Before arrival, no resolution.
        let acks = venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            50.0,
            Timestamp::from_nanos(500_000_000),
        );
        assert!(acks.is_empty());

        // By the time it "arrives," the quote it was aimed at is gone.
        let acks = venue.advance_market(
            contract(),
            0.90,
            100.0,
            0.95,
            50.0,
            Timestamp::from_nanos(1_000_000_000),
        );
        assert_eq!(acks.len(), 1);
        assert!(
            matches!(acks[0].status, AckStatus::Rejected { .. }),
            "expected the IOC to be cancelled on arrival, got {:?}",
            acks[0].status
        );
    }

    #[tokio::test]
    async fn queue_ahead_fraction_reduces_the_liquidity_our_resting_order_can_claim() {
        let venue = PaperAdapter::with_config(PaperConfig {
            queue_ahead_fraction: 0.5,
            latency_ns: 0,
            ..PaperConfig::default()
        });
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.70,
            100.0,
            Timestamp::from_nanos(0),
        );
        venue
            .submit(order(Side::Buy, 0.58, 20.0, OrderType::Limit))
            .await
            .unwrap();

        // 30 units of crossing liquidity arrive; half is assumed to be
        // ahead of us in the real queue, so only 15 are available to us —
        // less than our full 20-unit order.
        let fills = venue.advance_market(
            contract(),
            0.50,
            100.0,
            0.55,
            30.0,
            Timestamp::from_nanos(1),
        );
        assert_eq!(fills.len(), 1);
        match &fills[0].status {
            // advance_market reports each fill event's own quantity as
            // Filled, regardless of how much of the original order size
            // it represents — any remainder just keeps resting.
            AckStatus::Filled { qty, .. } => assert_eq!(*qty, 15.0),
            other => panic!("expected Filled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn find_order_by_client_id_has_no_record_of_an_order_never_submitted() {
        let venue = PaperAdapter::new();
        let never_submitted = order(Side::Buy, 0.5, 10.0, OrderType::ImmediateOrCancel);
        let id = parallax_types::ClientOrderId::derive(&never_submitted);
        assert_eq!(venue.find_order_by_client_id(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn find_order_by_client_id_returns_the_ack_from_an_immediate_fill() {
        let venue = PaperAdapter::new();
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );
        let intent = order(Side::Buy, 0.65, 20.0, OrderType::ImmediateOrCancel);
        let id = parallax_types::ClientOrderId::derive(&intent);
        let submitted_ack = venue.submit(intent).await.unwrap();

        let found = venue.find_order_by_client_id(&id).await.unwrap();
        assert_eq!(found, Some(submitted_ack));
    }

    #[tokio::test]
    async fn find_order_by_client_id_reflects_a_pending_orders_latest_status_not_its_first() {
        // A latency-delayed order is Accepted (into `pending`) on submit,
        // then resolved later by `activate_pending`. A retry orchestrator
        // querying by client id after the resolution must see the *final*
        // status, not the stale Accepted snapshot from submission time.
        let venue = PaperAdapter::with_config(PaperConfig {
            latency_ns: 1_000_000,
            ..PaperConfig::default()
        });
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(0),
        );
        let intent = order(Side::Buy, 0.65, 20.0, OrderType::ImmediateOrCancel);
        let id = parallax_types::ClientOrderId::derive(&intent);

        let submit_ack = venue.submit(intent).await.unwrap();
        assert_eq!(submit_ack.status, AckStatus::Accepted);
        assert_eq!(
            venue.find_order_by_client_id(&id).await.unwrap(),
            Some(submit_ack)
        );

        // The order's 1ms latency has now elapsed; this tick resolves it.
        venue.advance_market(
            contract(),
            0.60,
            100.0,
            0.63,
            100.0,
            Timestamp::from_nanos(2_000_000),
        );
        match venue.find_order_by_client_id(&id).await.unwrap() {
            Some(ack) => assert!(matches!(ack.status, AckStatus::Filled { .. })),
            None => panic!("expected the resolved fill's ack, found no record"),
        }
    }

    #[tokio::test]
    async fn a_paper_venue_reports_no_positions_by_design() {
        let venue = PaperAdapter::new();
        assert_eq!(venue.fetch_positions().await.unwrap(), Vec::new());
    }
}
