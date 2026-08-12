use crate::adapter::VenueAdapter;
use async_trait::async_trait;
use parallax_types::{
    AckStatus, CanonicalContractId, ExecError, OrderAck, OrderId, OrderIntent, OrderType,
    SettlementModel, Side, Timestamp, VenueCapabilities, VenueId,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy)]
struct MarketState {
    bid: f64,
    bid_size: f64,
    ask: f64,
    ask_size: f64,
}

struct PaperState {
    resting: HashMap<OrderId, OrderIntent>,
    market: HashMap<CanonicalContractId, MarketState>,
}

/// The in-memory simulated venue (design doc §4: "not a stub to be
/// deleted later, it is a first-class adapter target"). Implements real
/// limit-order-book matching semantics — a marketable limit crosses
/// immediately for whatever size is available and rests the remainder;
/// an IOC order fills whatever it can immediately and cancels the rest;
/// a passive resting limit fills later when `advance_market` reports a
/// tick that crosses it. This is what `parallax-sim` drives during
/// replay/backtest and what shadow mode drives during live bake-in.
pub struct PaperAdapter {
    state: Mutex<PaperState>,
    next_id: AtomicU64,
}

impl PaperAdapter {
    pub fn new() -> Self {
        PaperAdapter {
            state: Mutex::new(PaperState {
                resting: HashMap::new(),
                market: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    fn next_order_id(&self) -> OrderId {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        OrderId(format!("paper-{n}"))
    }

    fn try_immediate_match(market: Option<&MarketState>, intent: &OrderIntent) -> (f64, f64) {
        match (intent.side, market) {
            (Side::Buy, Some(m)) if intent.price >= m.ask => (intent.size.min(m.ask_size), m.ask),
            (Side::Sell, Some(m)) if intent.price <= m.bid => (intent.size.min(m.bid_size), m.bid),
            _ => (0.0, 0.0),
        }
    }

    /// Feed the venue's current best bid/ask for a contract — called by
    /// the sim harness on each replayed tick, or by a live market-data
    /// task in shadow mode. Returns any fills this update triggers
    /// against previously-resting orders.
    ///
    /// Resting orders competing for the same side are matched against a
    /// running remaining-liquidity counter (not the raw incoming size
    /// repeatedly) — otherwise two resting orders on the same side would
    /// each independently "see" the full quoted size and could jointly
    /// report filling far more than the venue actually offered. The
    /// depleted size is what gets stored as this tick's market state, so
    /// a later `submit()` call before the next tick sees what's actually
    /// left rather than the original undiminished quote.
    pub fn advance_market(
        &self,
        contract: CanonicalContractId,
        bid: f64,
        bid_size: f64,
        ask: f64,
        ask_size: f64,
        now: Timestamp,
    ) -> Vec<OrderAck> {
        let mut state = self.state.lock().unwrap();

        let candidate_ids: Vec<OrderId> = state
            .resting
            .iter()
            .filter(|(_, o)| o.contract == contract)
            .map(|(id, _)| id.clone())
            .collect();

        let mut remaining_ask_size = ask_size;
        let mut remaining_bid_size = bid_size;
        let mut fills = Vec::new();

        for id in candidate_ids {
            let intent = state.resting.get(&id).cloned().unwrap();
            let crosses = match intent.side {
                Side::Buy => ask <= intent.price,
                Side::Sell => bid >= intent.price,
            };
            if !crosses {
                continue;
            }
            let (fill_price, avail) = match intent.side {
                Side::Buy => (ask, &mut remaining_ask_size),
                Side::Sell => (bid, &mut remaining_bid_size),
            };
            let filled_qty = intent.size.min(*avail);
            if filled_qty <= 0.0 {
                continue;
            }
            *avail -= filled_qty;

            if filled_qty + 1e-9 < intent.size {
                let mut remainder = intent.clone();
                remainder.size -= filled_qty;
                state.resting.insert(id.clone(), remainder);
            } else {
                state.resting.remove(&id);
            }
            fills.push(OrderAck {
                order_id: id,
                venue: VenueId::Paper,
                status: AckStatus::Filled {
                    qty: filled_qty,
                    price: fill_price,
                },
                ts: now,
            });
        }

        state.market.insert(
            contract,
            MarketState {
                bid,
                bid_size: remaining_bid_size,
                ask,
                ask_size: remaining_ask_size,
            },
        );
        fills
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
            maker_fee_bps: 0.0,
            taker_fee_bps: 0.0,
            rate_limit_per_sec: u32::MAX,
        }
    }

    async fn submit(&self, order: OrderIntent) -> Result<OrderAck, ExecError> {
        let mut state = self.state.lock().unwrap();
        let market = state.market.get(&order.contract).copied();
        let (filled_qty, fill_price) = Self::try_immediate_match(market.as_ref(), &order);
        let order_id = self.next_order_id();

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

        match order.order_type {
            OrderType::ImmediateOrCancel => {
                if filled_qty <= 0.0 {
                    return Ok(OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status: AckStatus::Rejected {
                            reason: "no crossing liquidity available".into(),
                        },
                        ts: order.created_at,
                    });
                }
                let status = if filled_qty + 1e-9 >= order.size {
                    AckStatus::Filled {
                        qty: filled_qty,
                        price: fill_price,
                    }
                } else {
                    AckStatus::PartiallyFilled {
                        filled_qty,
                        remaining_qty: order.size - filled_qty,
                        price: fill_price,
                    }
                };
                Ok(OrderAck {
                    order_id,
                    venue: VenueId::Paper,
                    status,
                    ts: order.created_at,
                })
            }
            OrderType::Limit => {
                if filled_qty > 0.0 {
                    let remaining = order.size - filled_qty;
                    if remaining > 1e-9 {
                        let mut resting = order.clone();
                        resting.size = remaining;
                        state.resting.insert(order_id.clone(), resting);
                        Ok(OrderAck {
                            order_id,
                            venue: VenueId::Paper,
                            status: AckStatus::PartiallyFilled {
                                filled_qty,
                                remaining_qty: remaining,
                                price: fill_price,
                            },
                            ts: order.created_at,
                        })
                    } else {
                        Ok(OrderAck {
                            order_id,
                            venue: VenueId::Paper,
                            status: AckStatus::Filled {
                                qty: filled_qty,
                                price: fill_price,
                            },
                            ts: order.created_at,
                        })
                    }
                } else {
                    state.resting.insert(order_id.clone(), order.clone());
                    Ok(OrderAck {
                        order_id,
                        venue: VenueId::Paper,
                        status: AckStatus::Accepted,
                        ts: order.created_at,
                    })
                }
            }
        }
    }

    async fn cancel(&self, order_id: OrderId) -> Result<(), ExecError> {
        let mut state = self.state.lock().unwrap();
        if state.resting.remove(&order_id).is_some() {
            Ok(())
        } else {
            Err(ExecError::NotFound(order_id))
        }
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
}
