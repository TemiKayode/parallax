use crate::Bus;
use parallax_types::{FairValue, NormalizedTick, OrderAck, OrderIntent, RawEvent};

/// The concrete wiring of the architecture diagram in design doc §4: one
/// bounded topic per hop between pipeline stages. Every crate downstream
/// of ingestion talks to the pipeline only through this struct, never by
/// holding a reference to another stage directly — that boundary is what
/// lets `parallax-sim` swap real venue adapters for replay data without
/// the strategy core knowing the difference.
pub struct PipelineBus {
    /// Lossy: a dropped raw event is missed input, not corrupted state —
    /// the next observation supersedes it.
    pub raw_events: Bus<RawEvent>,
    /// Lossy: normalized venue quotes, one per book update. The next tick
    /// supersedes a dropped one.
    pub ticks: Bus<NormalizedTick>,
    /// Lossy: aggregated fair value + confidence band, one per recompute.
    /// The next recompute supersedes a dropped one.
    pub fair_values: Bus<FairValue>,
    /// Lossy: strategy engines' proposed orders, pre-risk-gate. A dropped
    /// proposal simply doesn't trade this tick.
    pub order_intents: Bus<OrderIntent>,
    /// Critical: venue responses, post-execution. A drop here means the
    /// position book never learns about a fill, and every risk check
    /// computed after that is wrong in a way nothing downstream can
    /// detect on its own — the error compounds rather than self-corrects
    /// (design doc review 1.9). See `PipelineBus::integrity_violated`.
    pub order_acks: Bus<OrderAck>,
}

pub struct PipelineBusConfig {
    pub raw_events_capacity: usize,
    pub ticks_capacity: usize,
    pub fair_values_capacity: usize,
    pub order_intents_capacity: usize,
    pub order_acks_capacity: usize,
}

impl Default for PipelineBusConfig {
    fn default() -> Self {
        // Ticks and fair values are the highest-frequency topics; order
        // flow is comparatively rare. Sized generously relative to
        // expected burst, not tuned to a specific deployment yet.
        PipelineBusConfig {
            raw_events_capacity: 4_096,
            ticks_capacity: 65_536,
            fair_values_capacity: 16_384,
            order_intents_capacity: 8_192,
            order_acks_capacity: 8_192,
        }
    }
}

impl PipelineBus {
    pub fn new(config: PipelineBusConfig) -> Self {
        PipelineBus {
            raw_events: Bus::new(config.raw_events_capacity),
            ticks: Bus::new(config.ticks_capacity),
            fair_values: Bus::new(config.fair_values_capacity),
            order_intents: Bus::new(config.order_intents_capacity),
            order_acks: Bus::new(config.order_acks_capacity),
        }
    }
}

impl Default for PipelineBus {
    fn default() -> Self {
        PipelineBus::new(PipelineBusConfig::default())
    }
}

impl PipelineBus {
    /// `true` once any `Critical` topic (currently only `order_acks`) has
    /// dropped at least one item. A caller — the backtest report, a live
    /// supervisor — should treat this as a reason to distrust every
    /// position/PnL number computed since, and the trading path should
    /// wire it to the kill switch rather than only logging it.
    pub fn integrity_violated(&self) -> bool {
        self.order_acks.dropped_count() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_is_fine_while_only_lossy_topics_drop() {
        let bus = PipelineBus::new(PipelineBusConfig {
            raw_events_capacity: 1,
            ticks_capacity: 1,
            fair_values_capacity: 1,
            order_intents_capacity: 1,
            order_acks_capacity: 1,
        });
        assert!(bus.ticks.try_publish(normalized_tick_stub()));
        assert!(!bus.ticks.try_publish(normalized_tick_stub()));
        assert!(!bus.integrity_violated());
    }

    #[test]
    fn integrity_is_violated_once_an_order_ack_drops() {
        let bus = PipelineBus::new(PipelineBusConfig {
            raw_events_capacity: 1,
            ticks_capacity: 1,
            fair_values_capacity: 1,
            order_intents_capacity: 1,
            order_acks_capacity: 1,
        });
        assert!(bus.order_acks.try_publish(order_ack_stub()));
        assert!(!bus.order_acks.try_publish(order_ack_stub()));
        assert!(bus.integrity_violated());
    }

    fn normalized_tick_stub() -> NormalizedTick {
        use parallax_types::{CanonicalContractId, Timestamp, VenueId};
        NormalizedTick {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("x".into()),
            bid: 0.5,
            bid_size: 1.0,
            ask: 0.6,
            ask_size: 1.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        }
    }

    fn order_ack_stub() -> OrderAck {
        use parallax_types::{AckStatus, OrderId, Timestamp, VenueId};
        OrderAck {
            order_id: OrderId("x".into()),
            venue: VenueId::Kalshi,
            status: AckStatus::Accepted,
            ts: Timestamp::from_nanos(0),
        }
    }
}
