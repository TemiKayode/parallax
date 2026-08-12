use crate::Bus;
use parallax_types::{FairValue, NormalizedTick, OrderAck, OrderIntent, RawEvent};

/// The concrete wiring of the architecture diagram in design doc §4: one
/// bounded topic per hop between pipeline stages. Every crate downstream
/// of ingestion talks to the pipeline only through this struct, never by
/// holding a reference to another stage directly — that boundary is what
/// lets `parallax-sim` swap real venue adapters for replay data without
/// the strategy core knowing the difference.
pub struct PipelineBus {
    /// Weather / econ / news / oracle facts, raw off the wire.
    pub raw_events: Bus<RawEvent>,
    /// Normalized venue quotes, one per book update.
    pub ticks: Bus<NormalizedTick>,
    /// Aggregated fair value + confidence band, one per recompute.
    pub fair_values: Bus<FairValue>,
    /// Strategy engines' proposed orders, pre-risk-gate.
    pub order_intents: Bus<OrderIntent>,
    /// Venue responses, post-execution.
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
