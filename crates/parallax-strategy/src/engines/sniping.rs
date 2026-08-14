use crate::core::{StrategyEngine, StrategyInput};
use parallax_types::{EngineId, OrderIntent, OrderType, Outcome, Side};

#[derive(Debug, Clone, Copy)]
pub struct SnipingConfig {
    pub max_order_size: f64,
    /// Minimum distance outside the band before this engine bothers —
    /// deliberately stricter than stat-arb's mere "outside band", since
    /// sniping commits to taking the entire resting quote immediately
    /// rather than sizing gradually into a position.
    pub min_edge: f64,
}

impl Default for SnipingConfig {
    fn default() -> Self {
        SnipingConfig {
            max_order_size: 300.0,
            min_edge: 0.03,
        }
    }
}

/// Triggers only as an immediate consequence of PARALLAX's own fresh
/// fair-value recompute finding a resting quote already mispriced
/// (design doc §8) — it takes exactly the size resting at that price via
/// an immediate-or-cancel order, rather than building a position
/// gradually the way stat-arb does. This is the one engine where the
/// internal decision-path latency (design doc §13) has a direct payoff:
/// the faster this recompute-to-order path runs, the more of the stale-
/// quote window gets captured before it updates or someone else takes it.
///
/// Requires the fair value to *postdate* the quote it's about to take
/// (design doc review 3.25): "price outside the band" alone is
/// indistinguishable from "our model hasn't caught up yet" — a fair
/// value computed *before* this tick arrived might simply be stale
/// relative to information the market has already priced in, in which
/// case PARALLAX is not the sniper here, it is the target. Only a fair
/// value recomputed at or after the tick's own timestamp counts as
/// confirmation that the disagreement is real.
pub struct LiquiditySnipingEngine {
    config: SnipingConfig,
}

impl LiquiditySnipingEngine {
    pub fn new(config: SnipingConfig) -> Self {
        LiquiditySnipingEngine { config }
    }
}

impl StrategyEngine for LiquiditySnipingEngine {
    fn id(&self) -> EngineId {
        EngineId::LiquiditySniping
    }

    fn evaluate(&self, input: &StrategyInput) -> Vec<OrderIntent> {
        let mut intents = Vec::new();

        for tick in input.book.quotes(input.contract) {
            if input.fair_value.as_of.since(tick.receive_ts) < 0 {
                // Our own information predates this quote — not a
                // confirmed mispricing, just us being behind.
                continue;
            }
            if tick.ask < input.fair_value.band_low
                && (input.fair_value.band_low - tick.ask) >= self.config.min_edge
            {
                let size = tick.ask_size.min(self.config.max_order_size);
                if size > 0.0 {
                    intents.push(OrderIntent {
                        venue: tick.venue,
                        contract: input.contract.clone(),
                        outcome: Outcome::Yes,
                        side: Side::Buy,
                        price: tick.ask,
                        size,
                        order_type: OrderType::ImmediateOrCancel,
                        engine: EngineId::LiquiditySniping,
                        created_at: input.now,
                    });
                }
            }
            if tick.bid > input.fair_value.band_high
                && (tick.bid - input.fair_value.band_high) >= self.config.min_edge
            {
                let size = tick.bid_size.min(self.config.max_order_size);
                if size > 0.0 {
                    intents.push(OrderIntent {
                        venue: tick.venue,
                        contract: input.contract.clone(),
                        outcome: Outcome::Yes,
                        side: Side::Sell,
                        price: tick.bid,
                        size,
                        order_type: OrderType::ImmediateOrCancel,
                        engine: EngineId::LiquiditySniping,
                        created_at: input.now,
                    });
                }
            }
        }
        intents
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::Calibrator;
    use parallax_book::ConsolidatedBook;
    use parallax_types::{
        CanonicalContractSpec, Direction, EventClass, FairValue, NormalizedTick, Timestamp, VenueId,
    };
    use std::collections::HashMap;

    fn contract() -> parallax_types::CanonicalContractId {
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

    fn fair_value_66_pm3() -> FairValue {
        FairValue {
            contract: contract(),
            midpoint: 0.66,
            band_low: 0.63,
            band_high: 0.69,
            as_of: Timestamp::from_nanos(0),
            inputs: vec![],
            effective_sample_size: 1.0,
        }
    }

    fn run(book: ConsolidatedBook) -> Vec<OrderIntent> {
        let inventory = HashMap::new();
        let calibration = Calibrator::default();
        let engine = LiquiditySnipingEngine::new(SnipingConfig::default());
        let fv = fair_value_66_pm3();
        let input = StrategyInput {
            contract: &contract(),
            fair_value: &fv,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(0),
        };
        engine.evaluate(&input)
    }

    #[test]
    fn snipes_exactly_the_resting_size_at_a_stale_cheap_quote() {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Polymarket,
            contract: contract(),
            bid: 0.55,
            bid_size: 20.0,
            ask: 0.58, // 0.63 - 0.58 = 0.05 >= min_edge 0.03
            ask_size: 42.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        let intents = run(book);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].order_type, OrderType::ImmediateOrCancel);
        assert_eq!(intents[0].size, 42.0);
    }

    #[test]
    fn edge_below_threshold_does_not_snipe() {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Polymarket,
            contract: contract(),
            bid: 0.60,
            bid_size: 20.0,
            ask: 0.615, // 0.63 - 0.615 = 0.015 < min_edge 0.03
            ask_size: 42.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        assert!(run(book).is_empty());
    }

    #[test]
    fn order_size_never_exceeds_available_resting_liquidity() {
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Polymarket,
            contract: contract(),
            bid: 0.55,
            bid_size: 20.0,
            ask: 0.50,
            ask_size: 5.0, // much less than max_order_size
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(0),
        });
        let intents = run(book);
        assert_eq!(intents[0].size, 5.0);
    }

    #[test]
    fn a_fair_value_that_predates_the_quote_does_not_snipe() {
        // Regression for design doc review 3.25: a mispricing-looking
        // quote must not be sniped off a fair value that hasn't caught up
        // to it yet — that's us being behind, not the market being wrong.
        let mut book = ConsolidatedBook::new();
        book.update(NormalizedTick {
            venue: VenueId::Polymarket,
            contract: contract(),
            bid: 0.55,
            bid_size: 20.0,
            ask: 0.58,
            ask_size: 42.0,
            venue_ts: None,
            receive_ts: Timestamp::from_nanos(1_000_000_000), // tick arrives at t=1s
        });
        let inventory = HashMap::new();
        let calibration = Calibrator::default();
        let engine = LiquiditySnipingEngine::new(SnipingConfig::default());
        let mut fv = fair_value_66_pm3();
        fv.as_of = Timestamp::from_nanos(0); // our view is from before the tick
        let input = StrategyInput {
            contract: &contract(),
            fair_value: &fv,
            book: &book,
            inventory: &inventory,
            calibration: &calibration,
            now: Timestamp::from_nanos(1_000_000_000),
        };
        assert!(engine.evaluate(&input).is_empty());
    }
}
